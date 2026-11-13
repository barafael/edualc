//! What a window needs to reach an edualcd and know it: its address, its certificate's
//! fingerprint (pinned: the certificate is edualcd's own, signed by no one), and the token it
//! lets windows in with. edualcd prints it as an address a browser opens too
//! (`https://host:port/#token=…&fingerprint=…`); `edualc --pair` takes the same. A demo
//! (`edualcd --demo`) is reached by its plain address instead (`http://host:port/`).

use std::fmt;

use sha2::Digest;

use super::token::Token;

/// The SHA-256 of a certificate (DER), as a browser shows it too.
#[derive(Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub fn of(certificate: &[u8]) -> Self {
        Self(sha2::Sha256::digest(certificate).into())
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} isn't a certificate's SHA-256 (64 hex digits)")]
pub struct NotAFingerprint(String);

impl std::str::FromStr for Fingerprint {
    type Err = NotAFingerprint;

    /// 64 hex digits, in either case, with or without colons between the bytes (as browsers
    /// show them).
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let wrong = || NotAFingerprint(text.to_owned());
        let digits: Vec<u8> = text.bytes().filter(|&byte| byte != b':').collect();
        let mut bytes = [0u8; 32];
        if digits.len() != bytes.len() * 2 {
            return Err(wrong());
        }
        let (pairs, _none_left) = digits.as_chunks::<2>();
        for (byte, pair) in bytes.iter_mut().zip(pairs) {
            let pair = std::str::from_utf8(pair).map_err(|_not_text| wrong())?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_not_hex| wrong())?;
        }
        Ok(Self(bytes))
    }
}

impl TryFrom<String> for Fingerprint {
    type Error = NotAFingerprint;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl From<Fingerprint> for String {
    fn from(fingerprint: Fingerprint) -> Self {
        fingerprint.to_string()
    }
}

/// An edualcd, as a window reaches it.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pairing {
    /// A name or an address, as the URL had it (an IPv6 address without its brackets).
    pub host: String,
    pub port: u16,
    pub fingerprint: Fingerprint,
    pub token: Token,
}

/// The token is left out: it lets whoever holds it run Claude Code as the user.
impl fmt::Debug for Pairing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pairing")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// Why an address doesn't pair.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PairingError {
    #[error(
        "an edualcd's address starts with https:// (edualcd prints it as it starts), a demo's \
         with http://"
    )]
    NotHttps,
    #[error("the address names no host and port")]
    NoAddress,
    #[error("{0:?} isn't a port")]
    NotAPort(String),
    #[error("the address has no token (#token=…)")]
    NoToken,
    #[error("the address has no fingerprint (…&fingerprint=…)")]
    NoFingerprint,
    #[error(transparent)]
    Fingerprint(#[from] NotAFingerprint),
}

impl Pairing {
    /// The address edualcd prints: the page's, which a window pairs with too.
    pub fn url(&self) -> String {
        format!(
            "https://{}/#token={}&fingerprint={}",
            self.authority(),
            encoded(self.token.as_str()),
            self.fingerprint
        )
    }

    /// `host:port`, an IPv6 address in brackets.
    pub fn authority(&self) -> String {
        authority(&self.host, self.port)
    }

    /// The line's address: the WebSocket's, and the protocol it speaks in its query. The token
    /// is not in it: the first frame of the line holds it (`Auth`).
    pub fn line_url(&self) -> String {
        format!(
            "wss://{}/api/line?protocol={}",
            self.authority(),
            super::PROTOCOL
        )
    }

    /// An address as edualcd prints it; what else its fragment holds (`dir`, say) is passed
    /// over.
    pub fn parse(url: &str) -> Result<Self, PairingError> {
        let rest = url
            .trim()
            .strip_prefix("https://")
            .ok_or(PairingError::NotHttps)?;
        let (host, port, fragment) = split_address(rest)?;
        let mut token = None;
        let mut fingerprint = None;
        for pair in fragment.split('&') {
            match pair.split_once('=') {
                Some(("token", value)) => token = Some(decoded(value)),
                Some(("fingerprint", value)) => fingerprint = Some(decoded(value).parse()?),
                Some(_) | None => {}
            }
        }
        Ok(Self {
            host,
            port,
            token: token
                .filter(|token| !token.is_empty())
                .map(Token::new)
                .ok_or(PairingError::NoToken)?,
            fingerprint: fingerprint.ok_or(PairingError::NoFingerprint)?,
        })
    }
}

/// An address's host, port and fragment, from what follows its scheme (`host:port/…#…`).
fn split_address(rest: &str) -> Result<(String, u16, &str), PairingError> {
    let (authority, fragment) = match rest.split_once('#') {
        Some((before, fragment)) => (before, fragment),
        None => (rest, ""),
    };
    let authority = authority.split('/').next().unwrap_or_default();
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, after) = bracketed.split_once(']').ok_or(PairingError::NoAddress)?;
        (
            host,
            after.strip_prefix(':').ok_or(PairingError::NoAddress)?,
        )
    } else {
        authority.rsplit_once(':').ok_or(PairingError::NoAddress)?
    };
    if host.is_empty() {
        return Err(PairingError::NoAddress);
    }
    let port = port
        .parse()
        .map_err(|_not_a_number| PairingError::NotAPort(port.to_owned()))?;
    Ok((host.to_owned(), port, fragment))
}

/// `host:port`, an IPv6 address in brackets.
fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// `text` as a URL's query or fragment may hold it: all but letters, digits and `-._~`
/// percent-encoded.
pub fn encoded(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else if write!(out, "%{byte:02X}").is_err() {
            tracing::debug!("a String refused a write");
        }
    }
    out
}

/// A value of a URL's query or fragment, its `%`-escapes undone.
pub fn decoded(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        let escaped = (byte == b'%')
            .then(|| bytes.get(at + 1..at + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(decoded) => {
                out.push(decoded);
                at += 3;
            }
            None => {
                out.push(if byte == b'+' { b' ' } else { byte });
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairing(host: &str) -> Pairing {
        Pairing {
            host: host.to_owned(),
            port: 7878,
            fingerprint: Fingerprint::of(b"a certificate"),
            token: Token::new("to ken/&".to_owned()),
        }
    }

    /// What edualcd prints, a window takes back whole, for names and both kinds of address.
    #[test]
    fn a_printed_address_pairs_back() {
        for host in ["box.local", "127.0.0.1", "::1"] {
            let pairing = pairing(host);
            let back = Pairing::parse(&pairing.url()).expect("the address pairs");
            assert_eq!(back.url(), pairing.url(), "{host}");
        }
        assert!(pairing("::1").url().starts_with("https://[::1]:7878/#"));
    }

    /// A browser's fingerprint (upper case, colons) is the same one; the page's `dir` is passed
    /// over.
    #[test]
    fn fingerprints_read_as_browsers_show_them() {
        let fingerprint = Fingerprint::of(b"a certificate");
        let shown = fingerprint
            .to_string()
            .to_uppercase()
            .as_bytes()
            .chunks(2)
            .map(|pair| String::from_utf8_lossy(pair).into_owned())
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(shown.parse(), Ok(fingerprint));
        let url = format!("https://h:1/#dir=%2Ftmp&token=t&fingerprint={shown}");
        assert_eq!(
            Pairing::parse(&url).map(|pairing| pairing.fingerprint),
            Ok(fingerprint)
        );
    }

    #[test]
    fn incomplete_addresses_are_refused() {
        let fingerprint = Fingerprint::of(b"x");
        let refused = |url: &str| Pairing::parse(url).map(|_| ());
        assert_eq!(refused("http://h:1/#token=t"), Err(PairingError::NotHttps));
        assert_eq!(
            refused(&format!("https://h/#token=t&fingerprint={fingerprint}")),
            Err(PairingError::NoAddress)
        );
        assert_eq!(
            refused(&format!("https://h:1/#fingerprint={fingerprint}")),
            Err(PairingError::NoToken)
        );
        assert_eq!(
            refused("https://h:1/#token=t"),
            Err(PairingError::NoFingerprint)
        );
        assert!(matches!(
            refused("https://h:1/#token=t&fingerprint=abc"),
            Err(PairingError::Fingerprint(_))
        ));
    }

    /// The token is in the address to pair with (its fragment, which no server is sent), and in
    /// no line's address, which proxies and servers log.
    #[test]
    fn the_token_is_in_no_address_a_line_goes_to() {
        let pairing = pairing("box");
        assert!(pairing.url().contains("token="));
        assert!(!pairing.line_url().contains("token"));
        assert!(!pairing.line_url().contains("to%20ken"));
    }

    #[test]
    fn the_token_stays_out_of_the_log() {
        assert!(!format!("{:?}", pairing("h")).contains("to ken"));
    }
}
