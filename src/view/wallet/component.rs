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
use gtk::{ApplicationWindow, ResponseType};
use relm::{init, Channel, Relm, StreamHandle, Update, Widget};

use super::{ElectrumState, ViewModel, Widgets};
use crate::model::{FileDocument, Wallet};
use crate::view::wallet::view_model::ModelError;
use crate::view::wallet::Msg;
use crate::view::{error_dlg, launch, pay, settings};
use crate::worker::{electrum, ElectrumWorker};

pub struct Component {
    model: ViewModel,
    widgets: Widgets,
    electrum_channel: Channel<electrum::Msg>,
    electrum_worker: ElectrumWorker,
    settings: relm::Component<settings::Component>,
    payment: relm::Component<pay::Component>,
    launcher_stream: Option<StreamHandle<launch::Msg>>,
}

impl Component {
    fn close(&self) {
        // TODO: Signal to launcher
        self.widgets.close();
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
                    .map(|stream| stream.emit(launch::Msg::OpenSelected));
            }
            Msg::Close => {
                self.launcher_stream
                    .as_ref()
                    .map(|stream| stream.emit(launch::Msg::WalletClosed));
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
            Msg::Pay => self.payment.emit(pay::Msg::Show),
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
                    Err(ModelError::Descriptor(err)) => error_dlg(
                        self.widgets.as_root(),
                        "Internal error",
                        "Please report the following information to the developer",
                        Some(&err.to_string()),
                    ),
                    Err(ModelError::FileSave(err)) => error_dlg(
                        self.widgets.as_root(),
                        "Error saving wallet",
                        "It was impossible to save changes to the wallet settings due to an error",
                        Some(&err.to_string()),
                    ),
                    Ok(new_server) => {
                        new_server.map(|electrum| self.widgets.update_electrum_server(&electrum));
                        self.widgets.show();
                        self.settings
                            .emit(settings::Msg::Response(ResponseType::Cancel));
                    }
                }
            }
            Msg::RegisterLauncher(stream) => {
                self.launcher_stream = Some(stream);
            }
            Msg::ElectrumWatch(msg) => self.handle_electrum(msg),
            _ => { /* TODO: Implement main window event handling */ }
        }
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

        let payment =
            init::<pay::Component>(model.to_wallet()).expect("error in settings component");
        payment.emit(pay::Msg::SetWallet(relm.stream().clone()));

        let stream = relm.stream().clone();
        let (electrum_channel, sender) =
            Channel::new(move |msg| stream.emit(Msg::ElectrumWatch(msg)));
        let electrum_worker = ElectrumWorker::with(sender, model.as_wallet().to_settings(), 60)
            .expect("unable to instantiate watcher thread");

        widgets.connect(relm);
        widgets.update_ui(&model);
        widgets.show();

        electrum_worker.sync();

        Component {
            model,
            widgets,
            settings,
            payment,
            electrum_channel,
            electrum_worker,
            launcher_stream: None,
        }
    }
}
