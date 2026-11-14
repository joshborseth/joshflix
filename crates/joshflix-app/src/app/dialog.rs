//! Modal confirmation dialogs.

use slint::{Global, Image, ModelRc, VecModel};

use super::{Controller, Key, shared};
use crate::{DetailButton, Dialog, Icons};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogAction {
    InstallUpdate,
    DismissUpdate,
    RemoveKey,
    Close,
    // The card and episode action sheet.
    RemoveFromContinue,
    MarkWatched,
    MarkEarlierWatched,
    MarkUnwatched,
    ToggleList,
}

pub struct DialogCtl {
    buttons: Vec<(String, Image, bool, DialogAction)>,
    index: usize,
}

impl Controller {
    pub fn show_dialog(&mut self, title: &str, body: &str, buttons: Vec<(&str, Image, bool, DialogAction)>) {
        let buttons: Vec<(String, Image, bool, DialogAction)> =
            buttons.into_iter().map(|(l, i, p, a)| (l.to_owned(), i, p, a)).collect();
        let state = Dialog::get(&self.ui);
        state.set_title(shared(title));
        state.set_body(shared(body));
        let model: Vec<DetailButton> = buttons
            .iter()
            .map(|(label, icon, primary, _)| DetailButton { label: shared(label), icon: icon.clone(), primary: *primary })
            .collect();
        state.set_buttons(ModelRc::new(VecModel::from(model)));
        state.set_index(0);
        state.set_sheet(false);
        state.set_open(true);
        self.dialog = Some(DialogCtl { buttons, index: 0 });
    }

    /// A list of actions, one per line.
    pub fn show_sheet(&mut self, title: &str, buttons: Vec<(&str, Image, bool, DialogAction)>) {
        self.show_dialog(title, "", buttons);
        Dialog::get(&self.ui).set_sheet(true);
    }

    fn close_dialog(&mut self) {
        self.dialog = None;
        Dialog::get(&self.ui).set_open(false);
    }

    pub fn dialog_key(&mut self, key: Key) -> bool {
        let Some(dialog) = self.dialog.as_mut() else { return true };
        match key {
            Key::Left | Key::Up => dialog.index = dialog.index.saturating_sub(1),
            Key::Right | Key::Down => dialog.index = (dialog.index + 1).min(dialog.buttons.len() - 1),
            Key::Back => {
                self.menu_target = None;
                let cancel = dialog.buttons.iter().find(|b| matches!(b.3, DialogAction::DismissUpdate | DialogAction::Close)).map(|b| b.3);
                self.close_dialog();
                if cancel == Some(DialogAction::DismissUpdate) {
                    self.dialog_action(DialogAction::DismissUpdate);
                }
                return true;
            }
            Key::Ok => {
                let action = dialog.buttons[dialog.index].3;
                self.close_dialog();
                self.dialog_action(action);
                return true;
            }
            _ => {}
        }
        Dialog::get(&self.ui).set_index(dialog.index as i32);
        true
    }

    fn dialog_action(&mut self, action: DialogAction) {
        match action {
            DialogAction::InstallUpdate => self.install_update(),
            DialogAction::DismissUpdate => {
                if let Some(update) = &self.update_available {
                    let code = update.manifest.version_code;
                    self.save_settings(|s| s.dismissed_update_code = Some(code));
                }
            }
            DialogAction::RemoveKey => {
                self.stop_playback(false);
                self.save_settings(|s| {
                    s.rd_api_key = None;
                    s.rd_username = None;
                    s.rd_premium_until = None;
                    s.onboarding_done = false;
                });
                self.toast("Real-Debrid key removed");
                self.show_onboarding();
            }
            DialogAction::Close => self.menu_target = None,
            DialogAction::RemoveFromContinue
            | DialogAction::MarkWatched
            | DialogAction::MarkEarlierWatched
            | DialogAction::MarkUnwatched
            | DialogAction::ToggleList => {
                self.sheet_action(action);
            }
        }
    }

    pub fn confirm_remove_key(&mut self) {
        let icons = Icons::get(&self.ui);
        self.show_dialog(
            "Remove your Real-Debrid key?",
            "Joshflix won't be able to play anything until you add a key again. Your list and watch progress stay on this TV.",
            vec![
                ("Remove key", icons.get_close(), true, DialogAction::RemoveKey),
                ("Cancel", icons.get_back(), false, DialogAction::Close),
            ],
        );
    }
}
