use std::{fmt::Debug, sync::Arc};
use brainz::music;
use chrono::TimeDelta;
use maybe_owned_string::MaybeOwnedString;
use musicdb::MusicDB;

use super::{StatusBackend, TimeDeltaExtension as _};

const FOUR_MINUTES: TimeDelta = TimeDelta::new(4 * 60, 0).unwrap();
const THIRTY_SECONDS: TimeDelta = TimeDelta::new(30, 0).unwrap();

use std::sync::LazyLock;
use lastfm::auth::ClientIdentity;

pub static DEFAULT_CLIENT_IDENTITY: LazyLock<ClientIdentity> = LazyLock::new(|| {
    ClientIdentity::new(
        concat!(
            clap::crate_name!(), "/",
            clap::crate_version!()
        ).to_owned(),
        "d591a37a79ec4c3d4efe55379029b5b3",
        "20a069921b30039bd2601d955e3bce46"
    ).expect("bad built-in client identity")
});

fn get_default_client_identity() -> ClientIdentity {
    DEFAULT_CLIENT_IDENTITY.clone()
}

fn is_default_client_identity(identity: &ClientIdentity) -> bool {
    identity == &*DEFAULT_CLIENT_IDENTITY
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Config {
    pub enabled: bool,
    #[serde(
        default = "get_default_client_identity",
        skip_serializing_if = "is_default_client_identity"
    )]
    pub identity: ClientIdentity,
    pub session_key: Option<lastfm::auth::SessionKey>
}

fn clean_album(mut str: &str) -> &str {
    for suffix in [
        " - Single",
        " - EP",
    ] {
        if str.ends_with(suffix) {
            str = &str[..str.len() - suffix.len()];
        }
    }
    str
}

#[derive(Debug)]
struct FirstArtistQuery<'a> {
    name: &'a str,
    id: musicdb::PersistentId<musicdb::Track<'a>>,
    artists: &'a str
}
impl<'a> From<&'a osa_apple_music::track::Track> for FirstArtistQuery<'a> {
    fn from(track: &'a osa_apple_music::track::Track) -> Self {
        Self {
            name: &track.name,
            id: musicdb::PersistentId::try_from(track.persistent_id.as_str()).expect("bad track persistent ID"),
            artists: track.artist.as_deref().unwrap_or_else(|| {
                tracing::error!("missing artist name for track w/ id {}", track.persistent_id);
                Default::default()
            })
        }
    }
}

/// Extracts a plausible "first" artist from a string that may contain multiple artists in the form "Artist1 & Artist2" or "Artist1, Artist2 & Artist3".
/// Uses external data sources (the iTunes store, ListenBrainz) to resolve conflicts.
// TODO: What if an artist uses a comma, like, in their name?
// TODO: Cache the results.
// TODO: Don't depend on the `listenbrainz` backend, which can be disabled with a feature flag.
async fn extract_first_artist<'a, 'b: 'a>(
    track: impl Into<FirstArtistQuery<'a>>,
    db: Option<&'b musicdb::MusicDB>,
    net: &reqwest::Client
) -> MaybeOwnedString<'a> {
    let track = Into::<FirstArtistQuery>::into(track);

    fn is_certainly_single_artist(id: &musicdb::PersistentId<musicdb::Track<'_>>, db: &musicdb::MusicDB) -> bool {
        if let Some(track) = db.get(*id) {
            if let Some(artist) = db.get(track.artist_id) {
                // Two combined artists are treated as one "artist" on the Apple Music frontend and in the MusicDB database.
                // However, this doesn't apply to the Apple Music / iTunes' backend.
                // This means we can use the lack of presence of a `cloud_catalog_id` to deduce whether an artist is "combined" or not,
                // with the caveat that it won't apply to local artists. In that instance, we'll just treat it as false and do some more digging.
                return artist.cloud_catalog_id.is_some()
            }
        }

        false
    }

    // TODO: Create a `brainz` abstraction.
    async fn search_listenbrainz(track: &FirstArtistQuery<'_>, net: &reqwest::Client) -> Option<String> {
        let query = format!("artist:\"{}\" AND recording:\"{}\"",
            urlencoding::encode(track.artists),
            urlencoding::encode(track.name)
        );
        
        use super::listenbrainz::DEFAULT_PROGRAM_INFO;
        let mut request = net.get("https://musicbrainz.org/ws/2/recording/")
            .header("User-Agent", &DEFAULT_PROGRAM_INFO.to_user_agent())
            .query(&[("query", query)]);

        let response = request.send().await.inspect_err(|err| {
            tracing::error!(?err, "failed to send request to ListenBrainz");
        }).ok()?;

        if !response.status().is_success() {
            tracing::error!(status = ?response.status(), "ListenBrainz API returned an error");
            return None
        }

        let response = response.text().await.inspect_err(|err| {
            tracing::error!(?err, "failed to read response from ListenBrainz");
        }).ok()?;

        use brainz::music::entities::Recording;
        let response: Recording = serde_json::from_str(&response).inspect_err(|err| {
            tracing::error!(?err, "failed to parse ListenBrainz response");
        }).ok()?;

        let mut credited = response.artist_credit.into_iter();
        Some(credited.next()?.artist.name)
    }

    // First, split by commas to go from "A, B, C & D" to just "A".
    // If it's in the form "A & B", it'll leave it as be, but that's fine and will be account for in a moment.
    let mut split_by_commas = track.artists.split(", ");
    let first = split_by_commas.next().unwrap_or(track.artists);
    let split_by_commas = split_by_commas.next().is_some();

    let mut split = first.split(" & ");
    let left = split.next().unwrap();

    if split.next().is_none() {
        return left.into()
    }

    // There are a two possible circumstances that need to be considered here:
    // - "A & B" is a single artist who happens to use an ampersand in their name, like "MYTH & ROID".
    // - "A & B" is two artists, "A" and "B".
    //    - "A" by themselves may or may not be in the MusicDB, but we want to return them regardless.

    if split_by_commas {
        // If we split by commas, then there are three or more artists, and the apostrophe for concatenating into a list
        // should've been at the back. That means this singular artist just has an ampersand in their name.
        // (i.e. we're in a situation like "A & B, C, D & E", and now know that "A & B" is a single artist since an apostrophe can't appear there for plain ol' lists.)
        return left.into()
    }

    if let Some(db) = db {
        if let Some(track) = db.get(track.id) {
            // So, the `cloud_catalog_artist_id` is the actual Apple Music ID for the artist.
            // Multiple client "artists" can map to that singular "real" artist; the real one, or any of the various collaboration artists.
            if let Some(cloud_artist_id) = track.numerics.cloud_catalog_artist_id {
                let matching_artists = db.artists().values().filter(|artist| artist.cloud_catalog_id == Some(cloud_artist_id)).collect::<Vec<_>>();
                // But we can know for certain that it *is* a single artist if we check their name and there isn't an ampersand in it.
                for artist in matching_artists {
                    if let Some(name) = &artist.name {
                        if !name.chars().any(|c| c == '&') {
                            return name.to_string().into()
                        }
                    }
                }

                // Well, we seemingly didn't have the original artist in the library, but
                // we can leverage the fact that an iTunes lookup will always return the singular
                // primary artist.
                if let Some(cloud) = itunes_api::lookup_artist(cloud_artist_id.into()).await.inspect_err(|err| {
                    tracing::error!(?err, "failed to lookup artist in iTunes API");
                }).ok().flatten() {
                    return cloud.name.into()
                }
            }
        }
    }

    // Without access to any more information, it's our best bet to just
    // send the track over to listenbrainz and see who they say the primary artist is.
    if let Some(artist) = search_listenbrainz(&track, net).await {
        return artist.into()
    }

    // Realistically, the artist is probably going to not have an ampersand in their name.
    // We'll just return the stuff to the left it.
    left.into()
}

/// Test the artist extraction function.
/// 
/// ## Environment
/// Requires to the following to be added to the library:
///  - "MYTH & ROID"'s "Endless Embrace"
///  - "CaptainSparklez & TryHardNinja"'s "Fallen Kingdom"
///  - "Satsuki, Hatsune Miku & Kasane Teto"'s "Mesmerizer"
///  - "The Age of Rockets"' "Pictures of Space"
#[tokio::test]
#[ignore = "requires suitable library"]
async fn artist_extraction () {
    let db = musicdb::MusicDB::default();
    let net = reqwest::Client::new();

    fn prepare_query<'a>(track_name: &'a str, artists: &'a str, db: &'a MusicDB) -> FirstArtistQuery<'a> {
        let artist_id = db.artists().values().find(|artist| artist.name.is_some_and(|v| v == artists)).unwrap_or_else(|| {
            panic!("missing required track for testing: artist(s) not found: \"{}\"", artists)
        }).persistent_id;
        let track  = db.tracks().values().find(|track| track.name.is_some_and(|v| v == track_name) && track.artist_id == artist_id).unwrap_or_else(|| {
            panic!("missing required track for testing: track not found: \"{}\" by \"{}\"", track_name, artists)
        });

        let track_id = track.persistent_id;
        FirstArtistQuery {
            name: track_name,
            id: track_id,
            artists
        }
    }


    // Has one artist. Nothing unusual.
    let pictures_of_space = prepare_query("Pictures of Space", "The Age of Rockets", &db);
    assert_eq!(extract_first_artist(pictures_of_space, Some(&db), &net).await, "The Age of Rockets".into());

    // Has one artist, but the artist has an ampersand in their name.
    let endless_embrace = prepare_query("Endless Embrace", "MYTH & ROID", &db);
    assert_eq!(extract_first_artist(endless_embrace, Some(&db), &net).await, "MYTH & ROID".into());

    // Has two artists; the first should be returned.
    let fallen_kingdom = prepare_query("Fallen Kingdom", "CaptainSparklez & TryHardNinja", &db);
    assert_eq!(extract_first_artist(fallen_kingdom, Some(&db), &net).await, "CaptainSparklez".into());

    // Has three artists; the first should be returned.
    let mesmerizer = prepare_query("Mesmerizer", "Satsuki, Hatsune Miku & Kasane Teto", &db);
    assert_eq!(extract_first_artist(mesmerizer, Some(&db), &net).await, "Satsuki".into());
} 

pub struct LastFM {
    client: ::lastfm::Client<::lastfm::auth::state::Authorized>
}
impl Debug for LastFM {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LastFM").finish()
    }
}
impl LastFM {
    pub fn new(identity: ClientIdentity, session_key: lastfm::auth::SessionKey) -> Self {
        let client = lastfm::Client::authorized(identity, session_key);
        Self { client }
    }

    /// Returns `None` if the track is missing required data (the artist or track name).
    async fn track_to_heard<'a>(track: &'a osa_apple_music::track::Track, artist: &'a str) -> Option<lastfm::scrobble::HeardTrackInfo<'a>> {
        Some(lastfm::scrobble::HeardTrackInfo {
            artist,
            track: &track.name,
            album: track.album.name.as_deref().map(clean_album),
            album_artist: if track.album.artist.as_ref().is_some_and(|aa| Some(aa) != track.artist.as_ref()) {
                // only sent if != track artist
                Some(track.album.artist.as_ref().unwrap())
            } else { None },
            duration_in_seconds: track.duration.map(|d| d as u32),
            track_number: track.track_number.map(|n| n.get() as u32),
            mbid: None
        })
    }
}

// TODO: Don't call `track_to_heard` twice (on start and on end).
#[async_trait::async_trait]
impl StatusBackend for LastFM {
    #[tracing::instrument(skip(self, context), level = "debug")]
    async fn record_as_listened(&self, context: super::BackendContext<()>) {
        let db = context.musicdb.as_ref().as_ref();
        let track = context.track.as_ref();
        let artist = extract_first_artist(track, db, &self.client.net).await;
        if let Some(info) = Self::track_to_heard(track, &artist).await {
            if let Err(error) = self.client.scrobble(&[lastfm::scrobble::Scrobble {
                chosen_by_user: None,
                timestamp: chrono::Utc::now(),
                info
            }]).await {
                tracing::error!(?error, "last.fm mark-listened failure")
            }
        } else {
            tracing::warn!("scrobble skipped; track is missing required data (artist name)")
        }
    }

    /// - <https://www.last.fm/api/scrobbling#scrobble-requests>
    async fn check_eligibility(&self, context: super::BackendContext<()>) -> bool {
        if let Some(duration) = context.track.duration {
            let length = TimeDelta::from_secs_f32(duration);
            let time_listened = context.listened.lock().await.total_heard();
            if length < THIRTY_SECONDS { return false };
            time_listened >= FOUR_MINUTES ||
            time_listened.as_secs_f32() >= (length.as_secs_f32() / 2.)
        } else { false }
    }

    #[tracing::instrument(skip(self, context), level = "debug")]
    async fn set_now_listening(&mut self, context: super::BackendContext<crate::data_fetching::AdditionalTrackData>) {
        let db = context.musicdb.as_ref().as_ref();
        let track = context.track.as_ref();
        let artist = extract_first_artist(track, db, &self.client.net).await;
        if let Some(info) = Self::track_to_heard(track, &artist).await {
            if let Err(error) = self.client.set_now_listening(&info).await {
                tracing::error!(?error, "last.fm now-listening dispatch failure")
            }
        } else {
            tracing::warn!("last.fm now-listening dispatch skipped; track is missing required data (artist name)")
        }
    }
}
