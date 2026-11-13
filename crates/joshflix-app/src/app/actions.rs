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
        let result = match (action, target) {
            (DialogAction::RemoveFromContinue, MenuTarget::Card(card)) => {
                store.hide_from_continue(&card.id).map(|()| "Removed from Continue Watching")
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
                    position_secs: existing.as_ref().map_or(0.0, |p| p.position_secs),
                    duration_secs: existing.as_ref().map_or(0.0, |p| p.duration_secs),
                    updated_at: 0,
                    up_next: false,
                    last_source: existing.and_then(|p| p.last_source),
                    hidden: false,
                    marked_from: None,
                };
                self.mark_episode_watched(progress)
            }
            (DialogAction::MarkUnwatched, MenuTarget::Card(card)) => {
                unmark(&store, &VideoRef::movie(&card.id)).map(|()| "Marked as unwatched")
            }
            (DialogAction::MarkUnwatched, MenuTarget::Episode { video }) => unmark(&store, &video).map(|()| "Marked as unwatched"),
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

impl Controller {
    /// An episode marked watched. The newest one moves the show on (its next
    /// aired episode becomes Up next, as after playback); an older one keeps
    /// the show's place in Continue Watching and on the Play button.
    fn mark_episode_watched(&mut self, progress: Progress) -> joshflix_core::Result<&'static str> {
        let store = self.svc.store.clone();
        let records = store.series_progress(&progress.video.meta_id)?;
        let newest = records.iter().max_by_key(|p| p.updated_at).is_none_or(|p| p.video == progress.video);
        if !newest {
            return store.save_progress_keep_time(watched(progress)).map(|()| "Marked as watched");
        }
        let meta = self.detail.as_ref().and_then(|d| d.meta.clone());
        let today = crate::format::today();
        let template = progress.clone();
        store.save_progress(watched(progress))?;
        // The show moves on to the first aired episode not yet watched: one
        // already started is brought forward as it is, an untouched one is
        // queued as Up next (as after playback).
        let Some(meta) = meta else { return Ok("Marked as watched") };
        let (Some(mut season), Some(mut episode)) = (template.video.season, template.video.episode) else {
            return Ok("Marked as watched");
        };
        while let Some(n) = meta.next_episode(season, episode).filter(|n| n.is_released(&today)).cloned() {
            let video = VideoRef::episode(&template.video.meta_id, n.season, n.episode);
            (season, episode) = (n.season, n.episode);
            let existing = store.progress(&video)?;
            if existing.as_ref().is_some_and(|e| e.is_watched()) {
                continue;
            }
            std::thread::sleep(std::time::Duration::from_millis(2)); // newer than the watched one
            match existing {
                Some(started) if !started.up_next => store.save_progress(Progress { hidden: false, ..started })?,
                _ => store.save_progress(Progress {
                    video,
                    episode_title: Some(n.title),
                    thumbnail: n.thumbnail,
                    position_secs: 0.0,
                    duration_secs: 0.0,
                    up_next: true,
                    last_source: None,
                    marked_from: None,
                    ..template
                })?,
            }
            break;
        }
        Ok("Marked as watched")
    }
}

/// "Mark as unwatched": back to where it stood before it was marked (or no
/// record), dropping the Up next that watching it queued, and keeping a show
/// that was taken off Continue Watching off it.
fn unmark(store: &joshflix_core::store::Store, video: &VideoRef) -> joshflix_core::Result<()> {
    let Some(record) = store.progress(video)? else { return Ok(()) };
    for queued in store.series_progress(&video.meta_id)? {
        if queued.up_next && queued.updated_at >= record.updated_at {
            store.remove_progress(&queued.video)?;
        }
    }
    match record.marked_from.filter(|at| *at > 0.0) {
        Some(at) => store.save_progress_keep_time(Progress { position_secs: at, marked_from: None, ..record.clone() })?,
        None => store.remove_progress(video)?,
    }
    if record.hidden {
        store.hide_from_continue(&video.meta_id)?;
    }
    Ok(())
}

/// Progress at the end of the video (an unknown length counts as one second).
fn watched(mut progress: Progress) -> Progress {
    let before = progress.position_secs;
    progress.duration_secs = progress.duration_secs.max(1.0);
    progress.marked_from = (before > 0.0 && before < progress.duration_secs).then_some(before);
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
        hidden: false,
        marked_from: None,
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
