//! The action sheet: hold OK (or press Menu) on a card or an episode for
//! Remove from Continue Watching, Mark as watched and My List.

use joshflix_core::store::{LaterEpisode, LibraryItem, Progress};
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
                self.set_pressed(true);
                Some(true)
            }
            (Key::Ok, true) => {
                if self.ok_pending == Some(false) {
                    self.ok_pending = Some(true);
                    self.set_pressed(false);
                    self.open_action_sheet();
                }
                Some(true)
            }
            (Key::OkUp, _) => {
                self.set_pressed(false);
                if self.ok_pending.take() == Some(false) {
                    return Some(self.dispatch_key(Key::Ok, false));
                }
                Some(true)
            }
            (Key::Menu, _) if self.ok_can_hold() => {
                self.ok_pending = None;
                self.set_pressed(false);
                self.open_action_sheet();
                Some(true)
            }
            _ => {
                // Another key before OK's release: the OK still counts, first.
                if self.ok_pending.take() == Some(false) {
                    self.set_pressed(false);
                    self.dispatch_key(Key::Ok, false);
                }
                None
            }
        }
    }

    fn set_pressed(&self, down: bool) {
        crate::Press::get(&self.ui).set_down(down);
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
        } else if card.kind == MediaKind::Movie {
            buttons.push(("Mark as unwatched", icons.get_close(), buttons.is_empty(), DialogAction::MarkUnwatched));
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
        // Store work runs off the UI thread (each write is an fsync on TV
        // flash); the toast and the refresh follow when it's done.
        let job: Box<dyn FnOnce() -> joshflix_core::Result<&'static str> + Send> = match (action, target) {
            (DialogAction::RemoveFromContinue, MenuTarget::Card(card)) => {
                Box::new(move || store.hide_from_continue(&card.id).map(|()| "Removed from Continue Watching"))
            }
            (DialogAction::MarkWatched, MenuTarget::Card(card)) => {
                let template = card_progress(&card);
                Box::new(move || store.mark_watched(template, &[]).map(|()| "Marked as watched"))
            }
            (DialogAction::MarkWatched, MenuTarget::Episode { video }) => {
                let Some(request) = self.play_request(video.clone(), None, None) else { return };
                let later = self.later_episodes(&video);
                let c = request.context;
                let template = Progress {
                    video,
                    name: c.name,
                    episode_title: c.episode_title,
                    poster: c.poster,
                    background: c.background,
                    thumbnail: c.thumbnail,
                    logo: c.logo,
                    position_secs: 0.0,
                    duration_secs: 0.0,
                    updated_at: 0,
                    up_next: false,
                    last_source: None,
                    hidden: false,
                    marked_from: None,
                    queued: None,
                    queued_prev_at: None,
                };
                Box::new(move || store.mark_watched(template, &later).map(|()| "Marked as watched"))
            }
            (DialogAction::MarkUnwatched, MenuTarget::Card(card)) => {
                Box::new(move || store.unmark_watched(&VideoRef::movie(&card.id)).map(|()| "Marked as unwatched"))
            }
            (DialogAction::MarkUnwatched, MenuTarget::Episode { video }) => {
                Box::new(move || store.unmark_watched(&video).map(|()| "Marked as unwatched"))
            }
            (DialogAction::ToggleList, MenuTarget::Card(card)) => Box::new(move || {
                if store.in_library(&card.id).unwrap_or(false) {
                    store.remove_from_library(&card.id).map(|_| "Removed from My List")
                } else {
                    store.add_to_library(library_item(&card)).map(|_| "Added to My List")
                }
            }),
            _ => return,
        };
        self.spawn(
            async move { tokio::task::spawn_blocking(job).await.unwrap_or_else(|e| Err(joshflix_core::CoreError::Invalid(e.to_string()))) },
            |c, result| {
                match result {
                    Ok(message) => c.toast(message),
                    Err(e) => c.toast(&format!("Couldn't save that: {e}")),
                }
                match c.screen {
                    Screen::Detail => c.refresh_detail_progress(),
                    Screen::Browse | Screen::Mylist => c.refresh_local_rows(),
                    _ => {}
                }
            },
        );
    }

    /// The aired episodes after `video`, in order, for "Mark as watched".
    fn later_episodes(&self, video: &VideoRef) -> Vec<LaterEpisode> {
        let (Some(meta), Some(mut season), Some(mut episode)) =
            (self.detail.as_ref().and_then(|d| d.meta.clone()), video.season, video.episode)
        else {
            return Vec::new();
        };
        let today = crate::format::today();
        let mut later = Vec::new();
        while let Some(next) = meta.next_episode(season, episode).filter(|n| n.is_released(&today)) {
            (season, episode) = (next.season, next.episode);
            later.push(LaterEpisode {
                video: VideoRef::episode(&video.meta_id, next.season, next.episode),
                title: Some(next.title.clone()),
                thumbnail: next.thumbnail.clone(),
            });
        }
        later
    }
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
        hidden: false,
        marked_from: None,
        queued: None,
        queued_prev_at: None,
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
