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

use gladis::Gladis;
use gtk::prelude::*;
use gtk::{
    ApplicationWindow, Button, Entry, FileChooserDialog, HeaderBar, Inhibit, Popover, Statusbar,
};
use relm::Relm;
use std::path::PathBuf;

use super::Msg;

// Create the structure that holds the widgets used in the view.
#[derive(Clone, Gladis)]
pub struct Widgets {
    window: ApplicationWindow,

    header_bar: HeaderBar,
    new_btn: Button,
    open_btn: Button,
    settings_btn: Button,

    status_bar: Statusbar,

    open_dlg: FileChooserDialog,

    invoice_popover: Popover,
    address_fld: Entry,
}

impl Widgets {
    pub fn show(&self) {
        self.window.show()
    }

    pub fn close(&self) {
        self.window.close()
    }

    pub fn to_root(&self) -> ApplicationWindow {
        self.window.clone()
    }
    pub fn as_root(&self) -> &ApplicationWindow {
        &self.window
    }

    pub fn show_open_dlg(&self) {
        self.open_dlg.show();
    }

    pub fn hide_open_dlg(&self) {
        self.open_dlg.hide();
    }

    pub fn selected_file(&self) -> Option<PathBuf> {
        self.open_dlg.filename()
    }

    pub(super) fn connect(&self, relm: &Relm<super::Component>) {
        connect!(relm, self.new_btn, connect_clicked(_), Msg::New);
        connect!(relm, self.open_btn, connect_clicked(_), Msg::Open);
        connect!(relm, self.settings_btn, connect_clicked(_), Msg::Settings);

        connect!(
            relm,
            self.open_dlg,
            connect_action_notify(_),
            Msg::OpenWallet
        );

        connect!(
            relm,
            self.window,
            connect_delete_event(_, _),
            return (None, Inhibit(false))
        );
    }
}