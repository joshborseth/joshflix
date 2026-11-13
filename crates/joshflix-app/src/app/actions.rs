//! The action sheet: hold OK (or press Menu) on a card or an episode for
//! Remove from Continue Watching, Mark as watched and My List.

use joshflix_core::store::{LibraryItem, Progress};
use joshflix_core::{MediaKind, VideoRef};
use slint::Global;

use super::browse::RowItem;
use super::dialog::DialogAction;
use super::{Controller, Key};
use crate::{Icons, Screen};

/// What the open action sheet acts on.
pub enum MenuTarget {
    Card(RowItem),
    Episode { video: VideoRef },
}

impl Controller {
    /// OK on a card waits for its release: held, it opens the action sheet
    /// instead. Returns `None` when the key isn't part of that gesture.
    pub(super) fn ok_hold_key(&mut self, key: Key, repeat: bool) -> Option<bool> {
        match (key, repeat) {
            (Key::Ok, false) if self.ok_can_hold() => {
                self.ok_pending = Some(false);
                Some(true)
            }
            (Key::Ok, true) => {
                if self.ok_pending == Some(false) {
                    self.ok_pending = Some(true);
                    self.open_action_sheet();
                }
                Some(true)
            }
            (Key::OkUp, _) => {
                if self.ok_pending.take() == Some(false) {
                    return Some(self.dispatch_key(Key::Ok, false));
                }
                Some(true)
            }
            (Key::Menu, _) if self.ok_can_hold() => {
                self.ok_pending = None;
                self.open_action_sheet();
                Some(true)
            }
            _ => {
                self.ok_pending = None;
                None
            }
        }
    }

    fn ok_can_hold(&self) -> bool {
        self.dialog.is_none()
            && !self.sources.open
            && self.sources.resolving.is_none()
            && (self.focused_card().is_some() || self.focused_episode().is_some())
    }

    fn focused_card(&self) -> Option<RowItem> {
        if self.rail_focus {
            return None;
        }
        match self.screen {
            Screen::Browse => self.browse[self.browse_index(self.section)].focused_item().cloned(),
            Screen::Discover => self.discover.focused_item().cloned(),
            Screen::Mylist => self.mylist.focused_item().cloned(),
            Screen::Search => self.search.focused_item().cloned(),
            _ => None,
        }
    }

    /// The focused episode card and whether it's watched.
    fn focused_episode(&self) -> Option<(VideoRef, String, bool)> {
        if self.screen != Screen::Detail {
            return None;
        }
        self.detail.as_ref()?.focused_episode()
    }

    fn open_action_sheet(&mut self) {
        let icons = Icons::get(&self.ui);
        if let Some((video, title, watched)) = self.focused_episode() {
            let mark = if watched {
                ("Mark as unwatched", icons.get_close(), true, DialogAction::MarkUnwatched)
            } else {
                ("Mark as watched", icons.get_check(), true, DialogAction::MarkWatched)
            };
            self.menu_target = Some(MenuTarget::Episode { video });
            self.show_sheet(&title, vec![mark, ("Cancel", icons.get_back(), false, DialogAction::Close)]);
            return;
        }
        let Some(card) = self.focused_card() else { return };
        let mut buttons = Vec::new();
        if card.progress_record.is_some() {
            buttons.push(("Remove from Continue Watching", icons.get_close(), true, DialogAction::RemoveFromContinue));
        }
        let watched = card.progress_record.as_ref().is_some_and(Progress::is_watched)
            || (card.kind == MediaKind::Movie
                && self.svc.store.progress(&VideoRef::movie(&card.id)).ok().flatten().is_some_and(|p| p.is_watched()));
        if card.kind == MediaKind::Movie && !watched {
            buttons.push(("Mark as watched", icons.get_check(), buttons.is_empty(), DialogAction::MarkWatched));
        }
        let in_list = self.svc.store.in_library(&card.id).unwrap_or(false);
        buttons.push(if in_list {
            ("Remove from My List", icons.get_close(), buttons.is_empty(), DialogAction::ToggleList)
        } else {
            ("Add to My List", icons.get_plus(), buttons.is_empty(), DialogAction::ToggleList)
        });
        buttons.push(("Cancel", icons.get_back(), false, DialogAction::Close));
        let title = card.title.clone();
        self.menu_target = Some(MenuTarget::Card(card));
        self.show_sheet(&title, buttons);
    }

    pub(super) fn sheet_action(&mut self, action: DialogAction) {
        let Some(target) = self.menu_target.take() else { return };
        let store = self.svc.store.clone();
        let result = match (action, target) {
            (DialogAction::RemoveFromContinue, MenuTarget::Card(card)) => {
                // Older in-progress episodes would surface next: clear them all.
                let mut result = Ok(());
                for _ in 0..64 {
                    let entry = store.continue_watching(500).ok().and_then(|cw| cw.into_iter().find(|p| p.video.meta_id == card.id));
                    let Some(entry) = entry else { break };
                    result = store.remove_progress(&entry.video);
                    if result.is_err() {
                        break;
                    }
                }
                result.map(|()| "Removed from Continue Watching")
            }
            (DialogAction::MarkWatched, MenuTarget::Card(card)) => {
                let progress = card_progress(&card);
                store.save_progress(watched(progress)).map(|()| "Marked as watched")
            }
            (DialogAction::MarkWatched, MenuTarget::Episode { video }) => {
                let Some(request) = self.play_request(video.clone(), None, None) else { return };
                let c = request.context;
                let existing = store.progress(&video).ok().flatten();
                let progress = Progress {
                    video,
                    name: c.name,
                    episode_title: c.episode_title,
                    poster: c.poster,
                    background: c.background,
                    thumbnail: c.thumbnail,
                    logo: c.logo,
                    position_secs: 0.0,
                    duration_secs: existing.as_ref().map_or(0.0, |p| p.duration_secs),
                    updated_at: 0,
                    up_next: false,
                    last_source: existing.and_then(|p| p.last_source),
                };
                store.save_progress(watched(progress)).map(|()| "Marked as watched")
            }
            (DialogAction::MarkUnwatched, MenuTarget::Episode { video }) => {
                store.remove_progress(&video).map(|()| "Marked as unwatched")
            }
            (DialogAction::ToggleList, MenuTarget::Card(card)) => {
                if store.in_library(&card.id).unwrap_or(false) {
                    store.remove_from_library(&card.id).map(|_| "Removed from My List")
                } else {
                    store.add_to_library(library_item(&card)).map(|_| "Added to My List")
                }
            }
            _ => return,
        };
        match result {
            Ok(message) => self.toast(message),
            Err(e) => self.toast(&format!("Couldn't save that: {e}")),
        }
        match self.screen {
            Screen::Detail => self.refresh_detail_progress(),
            Screen::Browse | Screen::Mylist => self.refresh_local_rows(),
            _ => {}
        }
    }
}

/// Progress at the end of the video (an unknown length counts as one second).
fn watched(mut progress: Progress) -> Progress {
    progress.duration_secs = progress.duration_secs.max(1.0);
    progress.position_secs = progress.duration_secs;
    progress.up_next = false;
    progress
}

fn card_progress(card: &RowItem) -> Progress {
    if let Some(p) = &card.progress_record {
        return p.clone();
    }
    let preview = card.target.preview.as_ref();
    Progress {
        video: VideoRef::movie(&card.id),
        name: card.title.clone(),
        episode_title: None,
        poster: preview.and_then(|p| p.poster.clone()),
        background: preview.and_then(|p| p.background.clone()),
        thumbnail: None,
        logo: preview.and_then(|p| p.logo.clone()),
        position_secs: 0.0,
        duration_secs: 0.0,
        updated_at: 0,
        up_next: false,
        last_source: None,
    }
}

fn library_item(card: &RowItem) -> LibraryItem {
    let preview = card.target.preview.as_ref();
    let record = card.progress_record.as_ref();
    LibraryItem {
        id: card.id.clone(),
        kind: card.kind,
        name: card.title.clone(),
        poster: preview.and_then(|p| p.poster.clone()).or_else(|| record.and_then(|p| p.poster.clone())),
        background: preview.and_then(|p| p.background.clone()).or_else(|| record.and_then(|p| p.background.clone())),
        logo: preview.and_then(|p| p.logo.clone()).or_else(|| record.and_then(|p| p.logo.clone())),
        release_info: preview.and_then(|p| p.release_info.clone()),
        added_at: 0,
    }
}
