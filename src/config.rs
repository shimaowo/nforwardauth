use hmac::Hmac;
use sha2::Sha256;

#[derive(Debug)]
pub struct Config {
    pub port: u16,
    pub auth_host: String,
    pub key: Hmac<Sha256>,
    pub cookie_secure: bool,
    pub cookie_domain: String,
    pub cookie_name: String,
    pub site_root: String,
}
