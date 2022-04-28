// MyCitadel desktop wallet: bitcoin & RGB wallet based on GTK framework.
//
// Written in 2022 by
//     Dr. Maxim Orlovsky <orlovsky@pandoraprime.ch>
//
// Copyright (C) 2022 by Pandora Prime Sarl, Switzerland.
//
// This software is distributed without any warranty. You should have received
// a copy of the AGPL-3.0 License along with this software. If not, see
// <https://www.gnu.org/licenses/agpl-3.0-standalone.html>.

use std::path::PathBuf;

use gladis::Gladis;
use gtk::prelude::*;
use gtk::{ApplicationWindow, ResponseType};
use relm::{init, Channel, Relm, StreamHandle, Update, Widget};

use ::wallet::descriptors::InputDescriptor;
use ::wallet::locks::{LockTime, SeqNo};
use ::wallet::psbt::{Construct, Psbt};
use ::wallet::scripts::PubkeyScript;
use bitcoin::blockdata::constants::WITNESS_SCALE_FACTOR;
use bitcoin::policy::DUST_RELAY_TX_FEE;
use bitcoin::secp256k1::SECP256K1;
use bitcoin::{EcdsaSighashType, Transaction, TxIn, TxOut};
use miniscript::DescriptorTrait;
use wallet::hd::UnhardenedIndex;

use super::pay::beneficiary_row::Beneficiary;
use super::{pay, ElectrumState, Msg, ViewModel, Widgets};
use crate::model::{FileDocument, Wallet};
use crate::view::{error_dlg, launch, settings, NotificationBoxExt};
use crate::worker::{electrum, ElectrumWorker};

pub struct Component {
    model: ViewModel,
    widgets: Widgets,
    pay_widgets: pay::Widgets,
    electrum_channel: Channel<electrum::Msg>,
    electrum_worker: ElectrumWorker,
    settings: relm::Component<settings::Component>,
    launcher_stream: Option<StreamHandle<launch::Msg>>,
}

impl Component {
    fn close(&self) {
        self.widgets.close();
        self.launcher_stream
            .as_ref()
            .map(|stream| stream.emit(launch::Msg::WalletClosed));
    }

    fn save(&mut self) {
        match self.model.save() {
            Ok(_) => {}
            Err(err) => error_dlg(
                self.widgets.as_root(),
                "Error saving wallet",
                "It was impossible to save changes to the wallet settings due to an error",
                Some(&err.to_string()),
            ),
        }
    }

    pub fn compose_psbt(&self) -> Result<(Psbt, UnhardenedIndex), pay::Error> {
        let wallet = self.model.as_wallet();

        let output_count = self.model.beneficiaries().n_items();
        let mut txouts = Vec::with_capacity(output_count as usize);
        let mut output_value = 0u64;
        for no in 0..output_count {
            let beneficiary = self
                .model
                .beneficiaries()
                .item(no)
                .expect("BeneficiaryModel is broken")
                .downcast::<Beneficiary>()
                .expect("BeneficiaryModel is broken");
            let script_pubkey = beneficiary.address()?.script_pubkey();
            let value = beneficiary.amount_sats()?;
            output_value += value;
            txouts.push(TxOut {
                script_pubkey,
                value,
            });
        }

        // TODO: Support constructing PSBTs from multiple descriptors (at descriptor-wallet lib)
        let (descriptor, _) = self.model.as_settings().descriptors_all()?;
        let lock_time = LockTime::since_now();
        let change_index = wallet.next_change_index();

        let fee_rate = self.model.fee_rate();
        let mut fee = DUST_RELAY_TX_FEE;
        let mut next_fee = fee;
        let mut prevouts = bset! {};
        let satisfaciton_weights = descriptor.max_satisfaction_weight()? as f32;
        let mut cycle_lim = 0usize;
        while fee <= DUST_RELAY_TX_FEE && fee != next_fee {
            fee = next_fee;
            prevouts = wallet
                .coinselect(output_value + fee as u64)
                .ok_or(pay::Error::InsufficientFunds)?
                .0;
            let txins = prevouts
                .iter()
                .map(|p| TxIn {
                    previous_output: p.outpoint,
                    script_sig: none!(),
                    sequence: 0, // TODO: Support spending from CSV outputs
                    witness: none!(),
                })
                .collect::<Vec<_>>();

            let tx = Transaction {
                version: 1,
                lock_time: lock_time.as_u32(),
                input: txins,
                output: txouts.clone(),
            };
            let vsize = tx.vsize() as f32 + satisfaciton_weights / WITNESS_SCALE_FACTOR as f32;
            next_fee = (fee_rate * vsize).ceil() as u32;
            cycle_lim += 1;
            if cycle_lim > 6 {
                return Err(pay::Error::FeeFailure);
            }
        }

        let inputs = prevouts
            .into_iter()
            .map(|prevout| InputDescriptor {
                outpoint: prevout.outpoint,
                terminal: prevout.terminal(),
                seq_no: SeqNo::default(), // TODO: Support spending from CSV outputs
                tweak: None,
                sighash_type: EcdsaSighashType::All, // TODO: Support more sighashes in the UI
            })
            .collect::<Vec<_>>();
        let outputs = txouts
            .into_iter()
            .map(|txout| (PubkeyScript::from(txout.script_pubkey), txout.value))
            .collect::<Vec<_>>();

        let psbt = Psbt::construct(
            &SECP256K1,
            &descriptor,
            lock_time,
            &inputs,
            &outputs,
            change_index,
            fee as u64,
            wallet,
        )?;

        Ok((psbt, change_index))
    }

    pub fn sync_pay(&self) -> Option<(Psbt, UnhardenedIndex)> {
        match self.compose_psbt() {
            Ok(psbt) => {
                self.pay_widgets.hide_message();
                Some(psbt)
            }
            Err(err) => {
                self.pay_widgets.show_error(&err.to_string());
                None
            }
        }
    }

    fn handle_electrum(&mut self, msg: electrum::Msg) {
        let wallet = self.model.as_wallet_mut();
        match msg {
            electrum::Msg::Connecting => {
                self.widgets
                    .update_electrum_state(ElectrumState::Connecting);
            }
            electrum::Msg::Connected => {
                self.widgets
                    .update_electrum_state(ElectrumState::QueryingBlockchainState);
            }
            electrum::Msg::LastBlock(block_info) => {
                self.widgets
                    .update_electrum_state(ElectrumState::RetrievingFees);
                wallet.update_last_block(&block_info);
                self.widgets.update_last_block(&block_info);
            }
            electrum::Msg::LastBlockUpdate(block_info) => {
                wallet.update_last_block(&block_info);
                self.widgets.update_last_block(&block_info);
            }
            electrum::Msg::FeeEstimate(f0, f1, f2) => {
                self.widgets
                    .update_electrum_state(ElectrumState::RetrievingHistory(0));
                wallet.update_fees(f0, f1, f2);
            }
            electrum::Msg::HistoryBatch(batch, no) => {
                self.widgets
                    .update_electrum_state(ElectrumState::RetrievingHistory(no as usize * 2));
                wallet.update_history(batch);
                self.widgets.update_history(&wallet.history());
            }
            electrum::Msg::UtxoBatch(batch, no) => {
                self.widgets
                    .update_electrum_state(ElectrumState::RetrievingHistory(no as usize * 2 + 1));
                wallet.update_utxos(batch);
                self.widgets.update_utxos(&wallet.utxos());
                self.widgets.update_state(wallet.state(), wallet.tx_count());
            }
            electrum::Msg::TxBatch(batch, progress) => {
                self.widgets
                    .update_electrum_state(ElectrumState::RetrievingTransactions(progress));
                wallet.update_transactions(batch);
                self.widgets.update_transactions(&wallet.transactions());
                self.widgets.update_state(wallet.state(), wallet.tx_count());
            }
            electrum::Msg::Complete => {
                self.widgets.update_addresses(&wallet.address_info());
                self.widgets.update_electrum_state(ElectrumState::Complete(
                    self.model.as_settings().electrum().sec,
                ));
                self.save();
            }
            electrum::Msg::Error(err) => {
                self.widgets
                    .update_electrum_state(ElectrumState::Error(err.to_string()));
            }
            electrum::Msg::ChannelDisconnected => {
                panic!("Broken electrum thread")
            }
        }
    }
}

impl Update for Component {
    // Specify the model used for this widget.
    type Model = ViewModel;
    // Specify the model parameter used to init the model.
    type ModelParam = PathBuf;
    // Specify the type of the messages sent to the update function.
    type Msg = Msg;

    fn model(relm: &Relm<Self>, path: Self::ModelParam) -> Self::Model {
        let wallet = Wallet::read_file(&path)
            .map_err(|err| {
                relm.stream()
                    .emit(Msg::FileError(path.clone(), err.to_string()))
            })
            .unwrap_or_default();
        ViewModel::with(wallet, path)
    }

    fn update(&mut self, event: Msg) {
        match event {
            Msg::New => {
                self.launcher_stream
                    .as_ref()
                    .map(|stream| stream.emit(launch::Msg::Show));
            }
            Msg::Open => {
                self.launcher_stream
                    .as_ref()
                    .map(|stream| stream.emit(launch::Msg::Wallet));
            }
            Msg::Close => self.close(),
            Msg::About => {
                self.launcher_stream
                    .as_ref()
                    .map(|stream| stream.emit(launch::Msg::About));
            }
            Msg::FileError(path, err) => {
                self.widgets.hide();
                error_dlg(
                    self.widgets.as_root(),
                    "Error opening wallet",
                    &path.display().to_string(),
                    Some(&err.to_string()),
                );
                self.close();
            }
            Msg::Pay(msg) => self.update_pay(msg),
            Msg::Settings => self.settings.emit(settings::Msg::View(
                self.model.to_settings(),
                self.model.path().clone(),
            )),
            Msg::Refresh => {
                self.electrum_worker.sync();
            }
            Msg::Update(signers, descriptor_classes, electrum) => {
                match self
                    .model
                    .update_descriptor(signers, descriptor_classes, electrum)
                {
                    Err(err) => error_dlg(
                        self.widgets.as_root(),
                        "Internal error",
                        "Please report the following information to the developer",
                        Some(&err.to_string()),
                    ),
                    Ok(new_server) => {
                        new_server.map(|electrum| self.widgets.update_electrum_server(&electrum));
                        self.widgets.show();
                        self.settings
                            .emit(settings::Msg::Response(ResponseType::Cancel));
                    }
                }
                self.save();
            }
            Msg::RegisterLauncher(stream) => {
                self.launcher_stream = Some(stream);
            }
            Msg::ElectrumWatch(msg) => self.handle_electrum(msg),
            _ => { /* TODO: Implement main window event handling */ }
        }
    }
}

impl Component {
    fn update_pay(&mut self, event: pay::Msg) {
        match event {
            pay::Msg::Show => {
                self.model.beneficiaries_mut().clear();
                self.model.beneficiaries_mut().append(&Beneficiary::new());
                self.pay_widgets.init_ui(&self.model);
                self.pay_widgets.show();
                return;
            }
            pay::Msg::Response(ResponseType::Ok) => {
                let (psbt, change_index) = match self.sync_pay() {
                    Some(data) => data,
                    None => return,
                };
                self.pay_widgets.hide();
                self.launcher_stream.as_ref().map(|stream| {
                    stream.emit(launch::Msg::CreatePsbt(
                        psbt,
                        self.model.as_settings().network(),
                    ))
                });
                // Update latest change index in wallet settings by sending message to the wallet component
                if self
                    .model
                    .as_wallet_mut()
                    .update_next_change_index(change_index)
                {
                    self.save();
                }
                return;
            }
            pay::Msg::Response(ResponseType::Cancel) => {
                self.pay_widgets.hide();
                return;
            }
            pay::Msg::Response(_) => {
                return;
            }
            _ => {} // Changes which update wallet tx
        }

        match event {
            pay::Msg::BeneficiaryAdd => {
                self.model.beneficiaries_mut().append(&Beneficiary::new());
            }
            pay::Msg::BeneficiaryRemove => {
                self.pay_widgets.selected_beneficiary_index().map(|index| {
                    self.model.beneficiaries_mut().remove(index);
                });
            }
            pay::Msg::SelectBeneficiary(index) => self.pay_widgets.select_beneficiary(index),
            pay::Msg::BeneficiaryEdit(index) => {
                self.pay_widgets.select_beneficiary(index);
                /* Check correctness of the model data */
            }
            pay::Msg::FeeChange => { /* Update fee and total tx amount */ }
            pay::Msg::FeeSetBlocks(_) => { /* Update fee and total tx amount */ }
            _ => {} // Changes which do not update wallet tx
        }

        self.sync_pay();
    }
}

impl Widget for Component {
    // Specify the type of the root widget.
    type Root = ApplicationWindow;

    // Return the root widget.
    fn root(&self) -> Self::Root {
        self.widgets.to_root()
    }

    fn view(relm: &Relm<Self>, model: Self::Model) -> Self {
        let glade_src = include_str!("wallet.glade");
        let widgets = Widgets::from_string(glade_src).expect("glade file broken");

        let settings = init::<settings::Component>(()).expect("error in settings component");
        settings.emit(settings::Msg::SetWallet(relm.stream().clone()));

        let stream = relm.stream().clone();
        let (electrum_channel, sender) =
            Channel::new(move |msg| stream.emit(Msg::ElectrumWatch(msg)));
        let electrum_worker = ElectrumWorker::with(sender, model.as_wallet().to_settings(), 60)
            .expect("unable to instantiate watcher thread");

        widgets.connect(relm);
        widgets.update_ui(&model);
        widgets.show();

        let glade_src = include_str!("pay/pay.glade");
        let pay_widgets = pay::Widgets::from_string(glade_src).expect("glade file broken");

        pay_widgets.connect(relm);
        pay_widgets.bind_beneficiary_model(relm, &model);
        pay_widgets.init_ui(&model);

        electrum_worker.sync();

        Component {
            model,
            widgets,
            pay_widgets,
            settings,
            electrum_channel,
            electrum_worker,
            launcher_stream: None,
        }
    }
}
