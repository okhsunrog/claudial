//! Credentials for the claude-proxy usage source.
//!
//! On a Plasma desktop we reuse the entries written by `claude-plasmoid`, so
//! the proxy password has one owner and is never copied to a plaintext config
//! file. Three environment variables provide an explicit portable fallback.

use std::env::VarError;

use anyhow::{Context, Result, anyhow, bail, ensure};
use zbus::{blocking::Connection, proxy};

pub(super) const URL_VAR: &str = "CLAUDIAL_PROXY_URL";
pub(super) const USERNAME_VAR: &str = "CLAUDIAL_PROXY_USERNAME";
pub(super) const PASSWORD_VAR: &str = "CLAUDIAL_PROXY_PASSWORD";

const APP_ID: &str = "claudial-host";
const FOLDER: &str = "claude-plasmoid";

#[proxy(
    interface = "org.kde.KWallet",
    default_service = "org.kde.kwalletd6",
    default_path = "/modules/kwalletd6"
)]
trait KWallet {
    #[zbus(name = "networkWallet")]
    fn network_wallet(&self) -> zbus::Result<String>;

    #[zbus(name = "isOpen")]
    fn is_open(&self, wallet: &str) -> zbus::Result<bool>;

    #[zbus(name = "open")]
    fn open(&self, wallet: &str, wid: i64, appid: &str) -> zbus::Result<i32>;

    #[zbus(name = "close")]
    fn close(&self, handle: i32, force: bool, appid: &str) -> zbus::Result<i32>;

    #[zbus(name = "readPassword")]
    fn read_password(
        &self,
        handle: i32,
        folder: &str,
        key: &str,
        appid: &str,
    ) -> zbus::Result<String>;
}

pub(super) struct Credentials {
    pub url: String,
    pub username: String,
    pub password: String,
}

pub(super) fn load() -> Result<Credentials> {
    let url = environment_value(URL_VAR)?;
    let username = environment_value(USERNAME_VAR)?;
    let password = environment_value(PASSWORD_VAR)?;

    match (url, username, password) {
        (Some(url), Some(username), Some(password)) => validate(Credentials {
            url,
            username,
            password,
        }),
        (None, None, None) => read_kwallet(),
        _ => bail!(
            "set all of {URL_VAR}, {USERNAME_VAR}, and {PASSWORD_VAR}, or unset all three to use KWallet"
        ),
    }
}

fn environment_value(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(anyhow!("{name} is not valid UTF-8")),
    }
}

fn read_kwallet() -> Result<Credentials> {
    let connection = Connection::session().context("connecting to the user D-Bus")?;
    let proxy = KWalletProxyBlocking::new(&connection).context("connecting to KWallet")?;
    let wallet = proxy
        .network_wallet()
        .context("finding the network wallet")?;

    ensure!(
        proxy.is_open(&wallet).context("checking KWallet state")?,
        "KWallet is locked; unlock it or configure the proxy variables"
    );
    let handle = proxy.open(&wallet, 0, APP_ID).context("opening KWallet")?;
    ensure!(handle >= 0, "KWallet is unavailable");

    let result: Result<Credentials> = (|| {
        Ok(Credentials {
            url: proxy
                .read_password(handle, FOLDER, "url", APP_ID)
                .context("reading proxy URL from KWallet")?,
            username: proxy
                .read_password(handle, FOLDER, "username", APP_ID)
                .context("reading proxy username from KWallet")?,
            password: proxy
                .read_password(handle, FOLDER, "password", APP_ID)
                .context("reading proxy password from KWallet")?,
        })
    })();
    let _ = proxy.close(handle, false, APP_ID);

    validate(result?)
}

fn validate(credentials: Credentials) -> Result<Credentials> {
    ensure!(
        !credentials.url.trim().is_empty()
            && !credentials.username.is_empty()
            && !credentials.password.is_empty(),
        "proxy credentials are incomplete"
    );
    Ok(credentials)
}
