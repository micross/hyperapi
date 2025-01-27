use super::{authenticator::GatewayAuthError, AuthProvider, AuthResult};
use crate::config::{ClientInfo, ConfigUpdate};
use hyper::http::request::Parts;
use jsonwebtoken::{decode, decode_header, errors, Algorithm, DecodingKey, Validation};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Mutex};
use tracing::{event, Level};

#[derive(Debug)]
pub struct JWTAuthProvider {
    apps: HashMap<String, ClientInfo>,
    token_cache: Mutex<LruCache<String, String>>,
}

impl AuthProvider for JWTAuthProvider {
    fn update_config(&mut self, update: ConfigUpdate) {
        match update {
            ConfigUpdate::ClientUpdate(client) => {
                let client_id = client.client_id.clone();
                self.apps.insert(client_id, client);
            }
            ConfigUpdate::ClientRemove(cid) => {
                self.apps.remove(&cid);
            }
            _ => {}
        }
    }

    fn identify_client(
        &self,
        head: Parts,
        service_id: &str,
    ) -> Result<(Parts, AuthResult), GatewayAuthError> {
        let token = Self::extract_token(&head)?;
        let client_id = Self::extract_kid(&token)?;
        let client = self
            .apps
            .get(&client_id)
            .ok_or(GatewayAuthError::UnknownClient)?;
        let sla = client
            .services
            .get(service_id)
            .ok_or(GatewayAuthError::InvalidSLA)?;

        // check cache
        let mut cache = self.token_cache.lock().unwrap();
        if let Some(cached_key) = cache.get(&token) {
            event!(
                Level::DEBUG,
                "cached data {} {}",
                cached_key,
                client.app_key
            );
            if cached_key.eq(&client.app_key) {
                return Ok((
                    head,
                    AuthResult {
                        client_id: client.client_id.clone(),
                        sla: sla.clone(),
                    },
                ));
            } else {
                return Err(GatewayAuthError::InvalidToken);
            }
        } else {
            Self::verify_token(token.clone(), &client.pub_key)?;
            cache.put(token, client.app_key.clone());
            return Ok((
                head,
                AuthResult {
                    client_id: client.client_id.clone(),
                    sla: sla.clone(),
                },
            ));
        }
    }
}

impl JWTAuthProvider {
    pub fn new() -> Self {
        JWTAuthProvider {
            apps: HashMap::new(),
            token_cache: Mutex::new(LruCache::new(1024)),
        }
    }

    fn extract_token(head: &Parts) -> Result<String, GatewayAuthError> {
        if let Some(token) = head.headers.get(hyper::header::AUTHORIZATION) {
            // find in authorization header
            let segs: Vec<&str> = token.to_str().unwrap().split(' ').collect();
            let token = *(segs.get(1).unwrap_or(&""));
            Ok(String::from(token))
        } else {
            Err(GatewayAuthError::TokenNotFound)
        }
    }

    fn extract_kid(token: &str) -> Result<String, GatewayAuthError> {
        let header = decode_header(token).unwrap();
        if let Some(kid) = header.kid {
            //println!("{:?}", kid);
            Ok(kid)
        } else {
            Err(GatewayAuthError::InvalidToken)
        }
    }

    fn verify_token(token: String, pubkey: &str) -> Result<(), GatewayAuthError> {
        let verify_key = DecodingKey::from_secret(pubkey.as_bytes());
        let validation = Validation::new(Algorithm::HS256);
        match decode::<JwtClaims>(&token, &verify_key, &validation) {
            Ok(_) => Ok(()),
            Err(err) => match *err.kind() {
                errors::ErrorKind::InvalidToken => Err(GatewayAuthError::InvalidToken),
                errors::ErrorKind::InvalidIssuer => Err(GatewayAuthError::InvalidIssuer),
                _ => Err(GatewayAuthError::InvalidToken),
            },
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JwtClaims {
    pub exp: usize, // Required (validate_exp defaults to true in validation). Expiration time (as UTC timestamp)
    pub iat: Option<u64>, // Optional. Issued at (as UTC timestamp)
    pub iss: Option<String>, // Optional. Issuer
    pub sub: String, // Optional. Subject (whom token refers to)
}
