//! Joshflix domain logic, independent of any UI.
//!
//! - [`cinemeta`]: catalogs, search and title metadata (Stremio's public Cinemeta service).
//! - [`comet`]: ranked Real-Debrid sources for a movie or episode (ElfHosted Comet).
//! - [`sources`]: the source model, ranking and next-episode matching.
//! - [`release`]: release-name parsing, filtering and ranking of those sources.
//! - [`mp4`]: whether an MP4 keeps its audio or captions far from its video.
//! - [`realdebrid`]: account checks and turning a torrent into a direct HTTPS stream.
//! - [`subtitles`]: subtitle lists from the keyless OpenSubtitles v3 service.
//! - [`store`]: on-device settings, My List and watch progress (redb).
//! - [`pairing`] and [`update`]: the private relay on the home network.
//! - [`images`]: poster/backdrop download, disk cache and downscaled decode.
//!
//! Every network call goes through one [`http::Http`] client (rustls with the
//! `ring` provider and Mozilla roots, so Android needs no Java verifier).

pub mod cinemeta;
pub mod comet;
pub mod error;
pub mod http;
pub mod ids;
pub mod mp4;
pub mod images;
pub mod pairing;
pub mod realdebrid;
pub mod release;
pub mod store;
pub mod sources;
pub mod subtitles;
pub mod update;

pub use error::{CoreError, Result};
pub use ids::{MediaKind, VideoRef};
