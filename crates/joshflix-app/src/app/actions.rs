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
            let mut buttons = vec![mark];
            // Catching up after watching elsewhere: everything before it too
            // (offered while some earlier episode isn't watched yet).
            if self.earlier_unwatched(&video) {
                buttons.push(("Mark watched up to here", icons.get_rewind(), false, DialogAction::MarkEarlierWatched));
            }
            buttons.push(("Cancel", icons.get_back(), false, DialogAction::Close));
            self.menu_target = Some(MenuTarget::Episode { video });
            self.show_sheet(&title, buttons);
            return;
        }
        let Some(card) = self.focused_card() else { return };
        let mut buttons = Vec::new();
        if card.progress_record.is_some() {
            buttons.push(("Remove from Continue Watching", icons.get_close(), true, DialogAction::RemoveFromContinue));
        }
        // A show's Continue Watching card stands for one episode: skip it from here.
        let episode_label = card
            .progress_record
            .as_ref()
            .filter(|p| card.kind == MediaKind::Series && !p.is_watched())
            .and_then(|p| p.video.episode_label())
            .map(|label| format!("Mark {} as watched", label.replace('E', " E")));
        if let Some(label) = &episode_label {
            buttons.push((label.as_str(), icons.get_check(), false, DialogAction::MarkWatched));
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
        type Job = Box<dyn FnOnce(&joshflix_core::store::Store) -> joshflix_core::Result<&'static str> + Send>;
        let job: Job = match (action, target) {
            (DialogAction::RemoveFromContinue, MenuTarget::Card(card)) => {
                Box::new(move |store| store.hide_from_continue(&card.id).map(|()| "Removed from Continue Watching"))
            }
            (DialogAction::MarkWatched, MenuTarget::Card(card)) if card.kind == MediaKind::Series => {
                // The show's Continue Watching card: mark its current episode.
                self.mark_show_card_watched(card);
                return;
            }
            (DialogAction::MarkWatched, MenuTarget::Card(card)) => {
                let template = card_progress(&card);
                Box::new(move |store| store.mark_watched(template, &[], &[]).map(|()| "Marked as watched"))
            }
            (DialogAction::MarkWatched | DialogAction::MarkEarlierWatched, MenuTarget::Episode { video }) => {
                let Some(template) = self.episode_template(&video) else { return };
                let later = self.later_episodes(&video);
                let earlier = if action == DialogAction::MarkEarlierWatched { self.earlier_episodes(&video) } else { Vec::new() };
                let message =
                    if earlier.is_empty() { "Marked as watched" } else { "Marked as watched up to here · Mark this one unwatched to undo" };
                Box::new(move |store| store.mark_watched(template, &earlier, &later).map(|()| message))
            }
            (DialogAction::MarkUnwatched, MenuTarget::Card(card)) => {
                Box::new(move |store| store.unmark_watched(&VideoRef::movie(&card.id)).map(|()| "Marked as unwatched"))
            }
            (DialogAction::MarkUnwatched, MenuTarget::Episode { video }) => {
                Box::new(move |store| store.unmark_watched(&video).map(|()| "Marked as unwatched"))
            }
            (DialogAction::ToggleList, MenuTarget::Card(card)) => Box::new(move |store| {
                if store.in_library(&card.id).unwrap_or(false) {
                    store.remove_from_library(&card.id).map(|_| "Removed from My List")
                } else {
                    store.add_to_library(library_item(&card)).map(|_| "Added to My List")
                }
            }),
            _ => return,
        };
        // On the store's writer thread, in order with playback's saves.
        self.store_write(job, |c, result| c.sheet_done(result));
    }

    fn sheet_done(&mut self, result: joshflix_core::Result<&'static str>) {
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

    /// A show's Continue Watching card: look the show up, then mark the
    /// episode the card stands for (moving the show on to the next).
    fn mark_show_card_watched(&mut self, card: RowItem) {
        let Some(record) = card.progress_record.clone() else { return };
        let label = record.video.episode_label().unwrap_or_default().replace('E', " E");
        self.toast(&format!("Marking {label} as watched…"));
        let cinemeta = self.svc.cinemeta.clone();
        let id = card.id.clone();
        self.spawn(async move { cinemeta.meta(MediaKind::Series, &id).await }, move |c, meta| {
            let Ok(meta) = meta else {
                c.toast("Couldn't look up this show. Try again from its page.");
                return;
            };
            let later = later_in(&meta, &record.video);
            let template = Progress { position_secs: 0.0, up_next: false, ..record };
            c.store_write(move |store| store.mark_watched(template, &[], &later), move |c, result| {
                let failed = result.is_err();
                c.sheet_done(result.map(|()| "Marked as watched"));
                if !failed {
                    c.toast(&format!("Marked {label} as watched · Mark it unwatched on the show page to undo"));
                }
            });
        });
    }

    /// Titles and art for marking an episode of the open show.
    fn episode_template(&self, video: &VideoRef) -> Option<Progress> {
        let request = self.play_request(video.clone(), None, None)?;
        let c = request.context;
        Some(Progress {
            video: video.clone(),
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
            marked_prev_at: None,
            marked_earlier: Vec::new(),
        })
    }

    /// Whether some regular episode before `video` isn't watched yet.
    fn earlier_unwatched(&self, video: &VideoRef) -> bool {
        let Some(detail) = self.detail.as_ref() else { return false };
        let Some(meta) = detail.meta.as_deref() else { return false };
        let (Some(season), Some(episode)) = (video.season, video.episode) else { return false };
        meta.episodes
            .iter()
            .filter(|e| e.season != 0 && (e.season, e.episode) < (season, episode))
            .any(|e| !detail.progress_of(&e.id).is_some_and(Progress::is_watched))
    }

    /// The regular episodes before `video` in the open show, oldest first
    /// (one pass: the show's names and art are cloned once per episode).
    fn earlier_episodes(&self, video: &VideoRef) -> Vec<Progress> {
        let Some(template) = self.episode_template(video) else { return Vec::new() };
        let Some(meta) = self.detail.as_ref().and_then(|d| d.meta.clone()) else { return Vec::new() };
        let (Some(season), Some(episode)) = (video.season, video.episode) else { return Vec::new() };
        meta.episodes
            .iter()
            .filter(|e| e.season != 0 && (e.season, e.episode) < (season, episode))
            .map(|e| Progress {
                video: VideoRef::episode(&video.meta_id, e.season, e.episode),
                episode_title: Some(e.title.clone()),
                thumbnail: e.thumbnail.clone(),
                ..template.clone()
            })
            .collect()
    }

    /// The aired episodes after `video`, in order, for "Mark as watched".
    fn later_episodes(&self, video: &VideoRef) -> Vec<LaterEpisode> {
        self.detail.as_ref().and_then(|d| d.meta.clone()).map(|meta| later_in(&meta, video)).unwrap_or_default()
    }
}

/// The aired episodes after `video` in `meta`, in order.
pub(super) fn later_in(meta: &joshflix_core::cinemeta::MetaDetail, video: &VideoRef) -> Vec<LaterEpisode> {
    let (Some(mut season), Some(mut episode)) = (video.season, video.episode) else { return Vec::new() };
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
        marked_prev_at: None,
        marked_earlier: Vec::new(),
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
