#![allow(unused)]

use auth::AccountCredentials;
use maybe_owned_string::MaybeOwnedString;
use serde::Deserialize;
pub mod auth;
pub mod scrobble;
mod parameters;


pub(crate) const API_URL: &str = "https://ws.audioscrobbler.com/2.0/";

pub struct Client<A: auth::state::AuthorizationStatus> {
    pub identity: auth::ClientIdentity,
    net: reqwest::Client,
    session_key: Option<auth::SessionKey>,
    _authorized: core::marker::PhantomData<A>
}
impl<A: auth::state::AuthorizationStatus> Client<A> {
    pub const fn is_authorized(&self) -> bool {
        self.session_key.is_some()
    }
}
impl Client<auth::state::Unauthorized> {
    pub fn new(identity: auth::ClientIdentity) -> Client<auth::state::Unauthorized> {
        Client::<auth::state::Unauthorized> {
            net: reqwest::Client::builder().user_agent(&identity.user_agent).build().expect("cannot construct reqwest client"),
            identity,
            session_key: None,
            _authorized: core::marker::PhantomData
        }
    }

    pub fn into_authorized(self, session_key: auth::SessionKey) -> Client<auth::state::Authorized> {
        Client::<auth::state::Authorized> {
            net: self.net,
            identity: self.identity,
            session_key: Some(session_key),
            _authorized: core::marker::PhantomData,
        }
    }
}
impl<'a> Client<auth::state::Authorized> {
    pub fn authorized(identity: auth::ClientIdentity, session_key: auth::SessionKey) -> Self {
        Self {
            net: reqwest::Client::builder().user_agent(&identity.user_agent).build().expect("cannot construct reqwest client"),
            identity,
            session_key: Some(session_key),
            _authorized: core::marker::PhantomData,
        }
    }

    pub const fn session_key(&self) -> &auth::SessionKey {
        self.session_key.as_ref().expect("no session key on client with authenticated type-state")
    }

    async fn dispatch_authorized<'b: 'a>(&'b self, mut request: ApiRequest<'a>) -> Result<reqwest::Response, reqwest::Error> {
        request.parameters.add("sk".to_string(), MaybeOwnedString::Borrowed(self.session_key().as_ref()));
        request.parameters.add("method".to_string(), MaybeOwnedString::Borrowed(request.endpoint));
        request.parameters.add("api_key".to_string(), MaybeOwnedString::Borrowed(self.identity.get_key()));
        request.parameters.add("api_sig".to_string(), MaybeOwnedString::Owned(request.parameters.sign(self.session_key(), &self.identity).to_string()));
        request.parameters.add("format".to_string(), MaybeOwnedString::Borrowed("json"));
        let request = self.net.request(request.method, crate::API_URL)
            .header("Content-Length", "0")
            .header("User-Agent", &self.identity.user_agent)
            .query(&request.parameters)
            .build()?;
        self.net.execute(request).await
    }


    pub async fn scrobble(&self, scrobbles: &[scrobble::Scrobble<'a>]) -> reqwest::Result<scrobble::response::ScrobbleServerResponse> {
        let response = self.dispatch_authorized(ApiRequest {
            endpoint: "track.scrobble",
            method: reqwest::Method::POST,
            parameters: scrobbles.into(),
        }).await?;

        let response = response.text().await.unwrap();
        let response = scrobble::response::ScrobbleServerResponse::new(response, scrobbles.len());
        
        Ok(response)
    }

    pub async fn set_now_listening(&self, track: &scrobble::HeardTrackInfo<'_>) -> reqwest::Result<String> {
        let response = self.dispatch_authorized(ApiRequest {
            endpoint: "track.updateNowPlaying",
            method: reqwest::Method::POST,
            parameters: track.into(),
        }).await?;

        let response = response.text().await.unwrap();
        
        Ok(response)
    }
}

struct ApiRequest<'a> {
    /// Called the "method" (as in method of a service) by Last.fm
    endpoint: &'static str,
    method: reqwest::Method,
    parameters: parameters::Map<'a>
}

#[derive(thiserror::Error, Debug)]

pub enum GeneralError {
    #[error("invalid service")]
    /// The service does not exist.
    InvalidService, // 2
    #[error("invalid method")]
    /// The service does not have the requested method.
    InvalidMethod, // 3
    #[error("authentication failed")]
    /// Lacking required permissions.
    AuthenticationFailure, // 4

    /// Missing a required parameter.
    #[error("missing required parameter")]
    MissingParameter, // 6

    /// An invalid resource was specified.
    #[error("invalid resource specified")]
    InvalidResource, // 7

    /// An unknown error occurred.
    #[error("unknown error")]
    UnknownError, // 8

    /// An invalid session key was utilized.
    #[error("session key is invalid")]
    InvalidSessionKey, // 9

    /// An invalid API key was utilized.
    #[error("api key is invalid")]
    InvalidApiKey, // 10
    
    /// The service is temporarily offline. Trying again later may result in success.
    #[error("service is offline")]
    ServiceOffline, // 11

    /// An invalid method signature was supplied.
    #[error("invalid method signature")]
    InvalidSignature, // 13

    /// A temporary error occurred while processing the request. Trying again later may result in success.
    #[error("a temporary error occurred")]
    TemporaryError, // 16

    /// The client has been suspended for abuse of the API. Contact last.fm support.
    #[error("client suspended")]
    SuspendedApiKey, // 26

    /// Ratelimit exceeded. Slow down a bit.
    #[error("ratelimit exceeded")]
    RatelimitExceeded, // 29
}
impl TryFrom<u8> for GeneralError {
    type Error = ();
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            2 => Ok(Self::InvalidService),
            3 => Ok(Self::InvalidMethod),
            4 => Ok(Self::AuthenticationFailure),
            6 => Ok(Self::MissingParameter),
            7 => Ok(Self::InvalidResource),
            8 => Ok(Self::UnknownError),
            9 => Ok(Self::InvalidSessionKey),
            10 => Ok(Self::InvalidApiKey),
            11 => Ok(Self::ServiceOffline),
            13 => Ok(Self::InvalidSignature),
            16 => Ok(Self::TemporaryError), 
            26 => Ok(Self::SuspendedApiKey),
            29 => Ok(Self::RatelimitExceeded),
            _ => Err(())
        }
    }
}

