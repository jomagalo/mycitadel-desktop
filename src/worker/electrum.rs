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

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;
use std::{io, thread};

use amplify::Wrapper;
use bitcoin::{OutPoint, Transaction, Txid};
use chrono::{DateTime, NaiveDateTime, Utc};
use electrum_client::{Client as ElectrumClient, ElectrumApi, HeaderNotification};
use gtk::gdk;
use relm::Sender;
use wallet::address::AddressCompat;
use wallet::hd::{SegmentIndexes, UnhardenedIndex};
use wallet::scripts::PubkeyScript;

use crate::model::{ElectrumServer, Prevout, WalletSettings};

enum Cmd {
    Sync,
    Pull,
    Update(ElectrumServer),
}

pub enum Msg {
    Connecting,
    Connected,
    Complete,
    LastBlock(HeaderNotification),
    LastBlockUpdate(HeaderNotification),
    FeeEstimate(f64, f64, f64),
    HistoryBatch(Vec<HistoryTxid>, u16),
    UtxoBatch(Vec<UtxoTxid>, u16),
    TxBatch(BTreeMap<Txid, Transaction>, f32),
    ChannelDisconnected,
    Error(electrum_client::Error),
}

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash, Debug)]
#[derive(StrictEncode, StrictDecode)]
#[strict_encoding(repr = u8)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate", rename_all = "lowercase")
)]
pub enum HistoryType {
    Incoming,
    Outcoming,
    Change,
}

impl HistoryType {
    pub fn icon_name(self) -> &'static str {
        match self {
            HistoryType::Incoming => "media-playlist-consecutive-symbolic",
            HistoryType::Outcoming => "mail-send-symbolic",
            HistoryType::Change => "view-refresh-symbolic",
        }
    }

    pub fn color(self) -> gdk::RGBA {
        match self {
            HistoryType::Incoming => {
                gdk::RGBA::new(38.0 / 256.0, 162.0 / 256.0, 105.0 / 256.0, 1.0)
            }
            HistoryType::Outcoming => {
                gdk::RGBA::new(165.0 / 256.0, 29.0 / 256.0, 45.0 / 256.0, 1.0)
            }
            HistoryType::Change => gdk::RGBA::new(119.0 / 256.0, 118.0 / 256.0, 123.0 / 256.0, 1.0),
        }
    }
}

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash, Debug)]
#[derive(StrictEncode, StrictDecode)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate")
)]
pub struct HistoryTxid {
    pub txid: Txid,
    pub height: i32,
    #[cfg_attr(feature = "serde", serde(with = "serde_with::rust::display_fromstr"))]
    pub address: AddressCompat,
    pub index: UnhardenedIndex,
    pub ty: HistoryType,
}

impl HistoryTxid {
    pub fn date_time_est(self) -> DateTime<chrono::Local> {
        height_date_time_est(self.height)
    }

    pub fn mining_info(self) -> String {
        match self.height {
            -1 => s!("pending"),
            _ => format!("{}", self.date_time_est().format("%F %l %P")),
        }
    }
}

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash, Debug)]
#[derive(StrictEncode, StrictDecode)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate")
)]
pub struct UtxoTxid {
    pub txid: Txid,
    pub height: u32,
    pub vout: u32,
    pub value: u64,
    #[cfg_attr(feature = "serde", serde(with = "serde_with::rust::display_fromstr"))]
    pub address: AddressCompat,
    pub index: UnhardenedIndex,
    pub change: bool,
}

impl UtxoTxid {
    pub fn outpoint(&self) -> OutPoint {
        OutPoint::new(self.txid, self.vout)
    }

    pub fn date_time_est(self) -> DateTime<chrono::Local> {
        height_date_time_est(self.height as i32)
    }

    pub fn mining_info(self) -> String {
        match self.height {
            0 => s!("mempool"),
            _ => format!("{}", self.date_time_est().format("%F %l %P")),
        }
    }
}

impl From<&UtxoTxid> for Prevout {
    fn from(utxo: &UtxoTxid) -> Prevout {
        Prevout {
            outpoint: utxo.outpoint(),
            amount: utxo.value,
            change: utxo.change,
            index: utxo.index,
        }
    }
}

impl From<UtxoTxid> for Prevout {
    fn from(utxo: UtxoTxid) -> Prevout {
        Prevout::from(&utxo)
    }
}

pub struct ElectrumWorker {
    worker_thread: JoinHandle<()>,
    watcher_thread: JoinHandle<()>,
    tx: mpsc::Sender<Cmd>,
}

impl ElectrumWorker {
    pub fn with(
        sender: Sender<Msg>,
        mut wallet_settings: WalletSettings,
        interval: u64,
    ) -> Result<Self, io::Error> {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let worker_thread = thread::Builder::new().name(s!("electrum")).spawn(move || {
            let mut client = electrum_init(wallet_settings.electrum(), &sender);

            loop {
                let _ = match (&client, rx.recv()) {
                    (Some(_), Ok(Cmd::Update(electrum))) => {
                        wallet_settings.update_electrum(electrum);
                        client = electrum_init(wallet_settings.electrum(), &sender);
                        Ok(())
                    }
                    (Some(client), Ok(Cmd::Sync)) => {
                        electrum_sync(&client, &wallet_settings, &sender)
                    }
                    (Some(client), Ok(Cmd::Pull)) => client.block_headers_pop().map(|res| {
                        if let Some(last_block) = res {
                            sender
                                .send(Msg::LastBlockUpdate(last_block))
                                .expect("electrum watcher channel is broken");
                        }
                    }),
                    (None, Ok(_)) => {
                        /* Can't handle since no client avaliable */
                        Ok(())
                    }
                    (_, Err(_)) => {
                        sender
                            .send(Msg::ChannelDisconnected)
                            .expect("electrum channel is broken");
                        Ok(())
                    }
                }
                .map_err(|err| {
                    sender
                        .send(Msg::Error(err))
                        .expect("electrum channel is broken");
                });
            }
        })?;

        let sender = tx.clone();
        let watcher_thread = thread::Builder::new()
            .name(s!("blockwatcher"))
            .spawn(move || loop {
                thread::sleep(Duration::from_secs(interval));
                sender.send(Cmd::Pull).expect("Electrum thread is dead")
            })
            .expect("unable to start blockchain watching thread");

        Ok(ElectrumWorker {
            tx,
            worker_thread,
            watcher_thread,
        })
    }

    pub fn sync(&self) {
        self.cmd(Cmd::Sync)
    }

    pub fn pull(&self) {
        self.cmd(Cmd::Pull)
    }

    pub fn update(&self, server: ElectrumServer) {
        self.cmd(Cmd::Update(server))
    }

    fn cmd(&self, cmd: Cmd) {
        self.tx.send(cmd).expect("Electrum thread is dead")
    }
}

pub fn electrum_init(electrum: &ElectrumServer, sender: &Sender<Msg>) -> Option<ElectrumClient> {
    let config = electrum_client::ConfigBuilder::new()
        .timeout(Some(5))
        .expect("we do not use socks here")
        .build();
    ElectrumClient::from_config(&electrum.to_string(), config)
        .map_err(|err| {
            sender
                .send(Msg::Error(err))
                .expect("electrum channel is broken");
        })
        .ok()
}

pub fn electrum_sync(
    client: &ElectrumClient,
    wallet_settings: &WalletSettings,
    sender: &Sender<Msg>,
) -> Result<(), electrum_client::Error> {
    sender
        .send(Msg::Connecting)
        .expect("electrum watcher channel is broken");

    sender
        .send(Msg::Connected)
        .expect("electrum watcher channel is broken");

    let last_block = client.block_headers_subscribe()?;
    sender
        .send(Msg::LastBlock(last_block))
        .expect("electrum watcher channel is broken");

    let fee = client.batch_estimate_fee([1, 2, 3])?;
    sender
        .send(Msg::FeeEstimate(fee[0], fee[1], fee[2]))
        .expect("electrum watcher channel is broken");

    let network = bitcoin::Network::from(wallet_settings.network());

    let mut txids = bset![];
    let mut upto_index = map! { true => UnhardenedIndex::zero(), false => UnhardenedIndex::zero() };
    for change in [true, false] {
        let mut offset = 0u16;
        let mut upto = UnhardenedIndex::zero();
        *upto_index.entry(change).or_default() = loop {
            let spk = wallet_settings
                .script_pubkeys(change, offset..=(offset + 19))
                .map_err(|err| electrum_client::Error::Message(err.to_string()))?;
            let history_batch: Vec<_> = client
                .batch_script_get_history(spk.values().map(PubkeyScript::as_inner))?
                .into_iter()
                .zip(&spk)
                .flat_map(|(history, (index, script))| {
                    history.into_iter().map(move |res| HistoryTxid {
                        txid: res.tx_hash,
                        height: res.height,
                        address: AddressCompat::from_script(&script.clone().into(), network)
                            .expect("broken descriptor"),
                        index: *index,
                        ty: if change {
                            HistoryType::Change
                        } else {
                            HistoryType::Incoming /* TODO: do proper type classification */
                        },
                    })
                })
                .collect();
            if history_batch.is_empty() {
                break upto;
            } else {
                upto = history_batch
                    .iter()
                    .map(|item| item.index)
                    .max()
                    .unwrap_or_default();
            }
            txids.extend(history_batch.iter().map(|item| item.txid));
            sender
                .send(Msg::HistoryBatch(history_batch, offset))
                .expect("electrum watcher channel is broken");

            let utxos: Vec<_> = client
                .batch_script_list_unspent(spk.values().map(PubkeyScript::as_inner))?
                .into_iter()
                .zip(spk)
                .flat_map(|(utxo, (index, script))| {
                    utxo.into_iter().map(move |res| UtxoTxid {
                        txid: res.tx_hash,
                        height: res.height as u32,
                        vout: res.tx_pos as u32,
                        value: res.value,
                        address: AddressCompat::from_script(&script.clone().into(), network)
                            .expect("broken descriptor"),
                        index,
                        change,
                    })
                })
                .collect();
            txids.extend(utxos.iter().map(|item| item.txid));
            sender
                .send(Msg::UtxoBatch(utxos, offset))
                .expect("electrum watcher channel is broken");

            offset += 20;
        };
    }
    let txids = txids.into_iter().collect::<Vec<_>>();
    for (no, chunk) in txids.chunks(20).enumerate() {
        let txmap = chunk
            .iter()
            .copied()
            .zip(client.batch_transaction_get(chunk)?)
            .collect::<BTreeMap<_, _>>();
        sender
            .send(Msg::TxBatch(
                txmap,
                (no + 1) as f32 / txids.len() as f32 / 20.0,
            ))
            .expect("electrum watcher channel is broken");
    }

    sender
        .send(Msg::Complete)
        .expect("electrum watcher channel is broken");

    Ok(())
}

// TODO: Do a binary file indexed by height, representing date/time information for each height
pub fn height_date_time_est(height: i32) -> DateTime<chrono::Local> {
    if height <= 0 {
        return chrono::Local::now();
    }
    let reference_height = 733961;
    let reference_time = 1651158666;
    let height_diff = height - reference_height;
    let timestamp = reference_time + height_diff * 600;
    let block_time = NaiveDateTime::from_timestamp(timestamp as i64, 0);
    DateTime::<chrono::Local>::from(DateTime::<Utc>::from_utc(block_time, Utc))
}
