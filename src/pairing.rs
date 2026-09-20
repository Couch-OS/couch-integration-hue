//! Pairing: the conversation that turns a bridge address into a key Couch
//! keeps, and a certificate it will never stop checking.
//!
//! A Hue bridge is paired by asking it for an application key while somebody
//! is standing at it with a finger on the round button. Before the button is
//! pressed the bridge answers error 101; after it, once, it issues a key. So
//! the whole flow is one request repeated until it stops saying 101:
//!
//! ```text
//! step  -> POST /api {"devicetype":"couch#package-dev"}
//!       <- [{"error":{"type":101,...}}]        waiting: press the button
//! step  -> ...same...
//!       <- [{"success":{"username":"<key>"}}]  a key
//!       -> GET /api/0/config                   the bridge id
//!       -> GET /clip/v2/resource               proof the key works
//!       <- done
//! ```
//!
//! Three things make this more than a loop.
//!
//! **The certificate.** A bridge's certificate is issued by Signify's private
//! CA or is self-signed, and it names the bridge id rather than the address,
//! so no public root can verify it and no name check can either. The only
//! moment Couch can honestly decide to trust one is while a person is
//! physically at the bridge pressing its button. So the first handshake of
//! this flow fills an empty pin cell ([`couch_sdk::tls::Pin`]), every later
//! request in the same flow must present exactly that certificate, and the
//! certificate goes into the credential beside the key. A bridge that
//! presents a different one half way through ends the conversation.
//!
//! **The clock is the package's.** The host gives a dialog
//! `pairing.max_seconds` (120) and closes it; this flow gives up at
//! [`PAIR_BUDGET`] (115 seconds) so the person sees "not linked in time"
//! rather than a dialog that vanishes. A slow bridge is not a dead one: a
//! request that times out leaves the conversation running, and only a bridge
//! that cannot be reached at all ends it.
//!
//! **Nothing said out loud is a secret.** Every line this flow produces - the
//! prompt, the failure, the summary - is written here, in full, and the only
//! thing in any of them that comes from the bridge is the last six characters
//! of its id.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use couch_sdk::{PairFailure, PairFlow, PairInput, PairPrompt, PairStep};
use serde_json::json;

use crate::{adapter::HueSettings, credential::HueCredential, tls, Deadlines, Error, Hue, Result};

/// How long one pairing attempt may last. The manifest gives the dialog 120
/// seconds; this is deliberately just inside it, so the package is what ends
/// the conversation and the person is told why.
pub const PAIR_BUDGET: Duration = Duration::from_secs(115);
/// How long Couch waits before asking again. Two seconds is often enough to
/// notice the button was pressed and slow enough not to hammer a bridge that
/// is doing something else.
const POLL_MS: u32 = 2000;
/// How many requests in a row may go unanswered, before this conversation has
/// met a bridge at all, until it stops telling the person to press a button.
/// An address nothing lives at does not refuse a connection; it says nothing,
/// which looks exactly like a slow bridge. A bridge that has shown its
/// certificate once is a bridge, and is given the whole budget.
const SILENT_ATTEMPTS: u32 = 2;
/// What the bridge lists this remote as in its own app. `couch#package-dev`
/// says both what it is and that it is the packaged preview, so the owner can
/// find and delete it.
const DEVICE_TYPE: &str = "couch#package-dev";

/// Every line this flow can say. None of them contains a key, a certificate,
/// a path or anything else the bridge told us - except the last six
/// characters of the bridge id in the summary, which is what lets a person
/// with two bridges tell which one they just paired.
const PRESS: &str = "Press the round button on top of the Hue Bridge";
const NOT_IN_TIME: &str = "The Hue bridge was not linked in time. Start again and press its button";
const UNREACHABLE: &str = "No Hue bridge answered at that address";
const REFUSED: &str = "The Hue bridge would not link with Couch";
const UNREADABLE: &str = "The Hue bridge answered something Couch could not use";
const CHANGED: &str = "That bridge presented a different certificate. Start again";
const NO_KEY: &str = "The Hue bridge issued a key Couch cannot use";

/// What one `POST /api` came to.
enum Attempt {
    /// A key. The button had been pressed.
    Key(String),
    /// Error 101: nobody has pressed the button yet.
    NotPressed,
    /// The bridge is there and did not answer in time. Not fatal: a bridge
    /// under load is still a bridge, and the budget is what ends this.
    Slow,
    /// Nothing answered at that address.
    Unreachable,
    /// The bridge presented a certificate that is not the one this
    /// conversation started with.
    Changed,
    /// The bridge said no, or said something this package cannot read.
    Refused(&'static str),
    /// The bridge answered with an error of its own that is not "press the
    /// button": its number, which is safe to show and says what went wrong.
    Bridge(u16),
}

/// One pairing conversation with one bridge.
pub struct HueFlow {
    /// The normalised address, which is also what a successful flow asks
    /// Couch to save.
    base: String,
    /// Empty until the first handshake, the bridge's exact certificate
    /// afterwards, and shared with the agent that filled it.
    pin: Arc<Mutex<Vec<u8>>>,
    /// Set by the verifier when a handshake presented a certificate that is
    /// not the pinned one. rustls reports that as an ordinary I/O error, and
    /// a conversation has to tell it from a bridge being slow.
    refused: Arc<AtomicBool>,
    /// Taken away by [`PairFlow::cancel`], so a closed dialog leaves nothing
    /// holding a connection or a certificate.
    agent: Option<ureq::Agent>,
    started: Instant,
    budget: Duration,
    /// Requests in a row that nothing answered. See [`SILENT_ATTEMPTS`].
    silent: u32,
}

impl std::fmt::Debug for HueFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HueFlow").finish_non_exhaustive()
    }
}

impl HueFlow {
    /// A conversation with the bridge these settings address.
    pub fn new(settings: &HueSettings) -> Result<Self> {
        Self::with_budget(settings, PAIR_BUDGET)
    }

    /// [`HueFlow::new`] with its own deadline. The shipped package always
    /// passes [`PAIR_BUDGET`]; a test passes a short one, because the only
    /// honest way to prove a conversation ends on its budget is to let one
    /// end on its budget.
    pub fn with_budget(settings: &HueSettings, budget: Duration) -> Result<Self> {
        let base = crate::base(&settings.host)?;
        let pin = Arc::new(Mutex::new(Vec::new()));
        let refused = Arc::new(AtomicBool::new(false));
        Ok(Self {
            // Two seconds to connect and four in all: the budget for one
            // step of a conversation somebody is watching.
            agent: Some(tls::pair_agent(pin.clone(), refused.clone())),
            base,
            pin,
            refused,
            started: Instant::now(),
            budget,
            silent: 0,
        })
    }

    /// The certificate this conversation is pinned to, once a handshake has
    /// happened. Empty before that.
    pub fn certificate(&self) -> Vec<u8> {
        self.pin.lock().map(|pin| pin.clone()).unwrap_or_default()
    }

    /// What a request nobody answered means. Before any handshake it is
    /// counted, because nothing has shown there is a bridge to wait for.
    fn unanswered(&mut self) -> Attempt {
        if !self.certificate().is_empty() {
            return Attempt::Slow;
        }
        self.silent += 1;
        if self.silent >= SILENT_ATTEMPTS {
            Attempt::Unreachable
        } else {
            Attempt::Slow
        }
    }

    /// One `POST /api`.
    fn attempt(&self) -> Attempt {
        let Some(agent) = &self.agent else {
            return Attempt::Refused(REFUSED);
        };
        let sent = agent
            .post(format!("{}/api", self.base))
            // No client key: this package has no entertainment streaming, and
            // a key it does not use is a key it would have to keep. The field
            // is left out, not sent as false: a real bridge refuses
            // `"generateclientkey": false` as an invalid value (error 7) and
            // never gets as far as looking at its link button.
            .send_json(json!({"devicetype": DEVICE_TYPE}));
        let response = match sent {
            Ok(response) => response,
            // The bridge is there and did not answer in time. The budget is
            // what ends this, not one slow request.
            Err(ureq::Error::Timeout(_)) => return Attempt::Slow,
            // The certificate is the whole trust decision. rustls reports a
            // refusal as an alert, which arrives here as an ordinary I/O
            // error, so the verifier says so directly.
            Err(_) if self.refused.swap(false, Ordering::SeqCst) => return Attempt::Changed,
            Err(ureq::Error::Tls(_) | ureq::Error::Rustls(_)) => return Attempt::Changed,
            Err(
                ureq::Error::Io(_)
                | ureq::Error::ConnectionFailed
                | ureq::Error::HostNotFound
                | ureq::Error::BadUri(_),
            ) => return Attempt::Unreachable,
            Err(_) => return Attempt::Refused(UNREADABLE),
        };
        let Ok(value) = crate::response(response) else {
            return Attempt::Refused(UNREADABLE);
        };
        let Some(rows) = value.as_array() else {
            return Attempt::Refused(UNREADABLE);
        };
        for row in rows {
            // 101 is the one error that means "carry on asking".
            if row["error"]["type"] == 101 {
                return Attempt::NotPressed;
            }
            if let Some(error) = row.get("error") {
                return match error["type"].as_u64().and_then(|n| u16::try_from(n).ok()) {
                    Some(number) => Attempt::Bridge(number),
                    None => Attempt::Refused(REFUSED),
                };
            }
        }
        match rows
            .first()
            .and_then(|row| row["success"]["username"].as_str())
        {
            Some(key) if usable(key) => Attempt::Key(key.to_string()),
            Some(_) => Attempt::Refused(NO_KEY),
            None => Attempt::Refused(UNREADABLE),
        }
    }

    /// The bridge issued a key. Learn which bridge it was, prove the key
    /// works, and hand both over with the certificate the first handshake
    /// pinned.
    fn finish(&self, key: &str) -> Result<PairStep> {
        let certificate = self.certificate();
        if certificate.is_empty() {
            // A key with no handshake behind it cannot be pinned, so it
            // cannot be stored: there would be nothing to check next time.
            return Err(Error::Transport);
        }
        let agent = self.agent.as_ref().ok_or(Error::Transport)?;
        let config = crate::response(
            agent
                .get(format!("{}/api/0/config", self.base))
                .call()
                .map_err(crate::transport)?,
        )?;
        let bridge_id = config["bridgeid"]
            .as_str()
            .filter(|id| identifies(id))
            .ok_or(Error::Response)?;
        let credential = HueCredential::new(key, bridge_id, certificate);
        // One authenticated read, through a client pinned to that same
        // certificate: proof that the key works and that the credential about
        // to be stored is a credential that opens this bridge. A pairing that
        // hands Couch a key it has never used is a pairing that fails later,
        // in front of somebody who has already walked away.
        Hue::new_with(&self.base, key, &credential.certificate, Deadlines::Write)?
            .raw_resources()?;
        Ok(
            PairStep::done(credential.to_credential(), summary(bridge_id))
                // The address as this package will use it, so a person who
                // typed a bare IP gets back what was actually reached.
                .with_settings(json!({"host": self.base})),
        )
    }
}

/// One line naming what was paired. The last six characters of the bridge id
/// and nothing else: enough to tell two bridges apart, too little to be worth
/// anything to anyone who reads a log.
fn summary(bridge_id: &str) -> String {
    let tail: String = {
        let mut last: Vec<char> = bridge_id.chars().rev().take(6).collect();
        last.reverse();
        last.into_iter().collect()
    };
    format!("Paired with Hue bridge ...{tail}")
}

/// Whether a key is one this package could ever send as a header.
fn usable(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Whether a bridge id is one this package could ever store or show.
fn identifies(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

impl PairFlow for HueFlow {
    /// One step: ask the bridge once, and say what to do next. There is
    /// nothing to collect from the person, so `input` is never used.
    fn step(&mut self, _input: Option<PairInput>) -> couch_sdk::Result<PairStep> {
        // The clock is checked first, so a conversation that has run out
        // costs the bridge nothing.
        if self.started.elapsed() > self.budget {
            return Ok(PairStep::failed(PairFailure::TimedOut).because(NOT_IN_TIME));
        }
        let attempt = match self.attempt() {
            Attempt::Slow => self.unanswered(),
            other => {
                self.silent = 0;
                other
            }
        };
        Ok(match attempt {
            Attempt::Key(key) => match self.finish(&key) {
                Ok(step) => step,
                // The key arrived and something after it did not. Nothing is
                // stored: a half-finished pairing is not a pairing.
                Err(Error::Authentication) => {
                    PairStep::failed(PairFailure::Refused).because(NO_KEY)
                }
                Err(Error::Transport) => {
                    PairStep::failed(PairFailure::Unreachable).because(UNREACHABLE)
                }
                Err(_) => PairStep::failed(PairFailure::Refused).because(UNREADABLE),
            },
            Attempt::NotPressed => {
                PairStep::waiting(PairPrompt::press_button().saying(PRESS), POLL_MS)
            }
            // A bridge that is there and slow keeps the conversation alive;
            // the budget above is what ends it.
            Attempt::Slow => PairStep::waiting(PairPrompt::press_button().saying(PRESS), POLL_MS),
            Attempt::Unreachable => PairStep::failed(PairFailure::Unreachable).because(UNREACHABLE),
            Attempt::Changed => PairStep::failed(PairFailure::Refused).because(CHANGED),
            Attempt::Refused(message) => PairStep::failed(PairFailure::Refused).because(message),
            Attempt::Bridge(number) => PairStep::failed(PairFailure::Refused)
                .because(format!("{REFUSED} (Hue error {number})")),
        })
    }

    /// The person closed the dialog. Drop the agent and forget the
    /// certificate: a cancelled pairing leaves nothing behind, and there is
    /// nothing to tell a Hue bridge (it was never told this had started).
    fn cancel(&mut self) {
        self.agent = None;
        if let Ok(mut pin) = self.pin.lock() {
            pin.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_names_the_bridge_without_saying_anything_worth_knowing() {
        assert_eq!(
            summary("001788FFFE0A1B2C"),
            "Paired with Hue bridge ...0A1B2C"
        );
        // Short and odd ids still produce a line, and never a panic.
        assert_eq!(summary("abc"), "Paired with Hue bridge ...abc");
        assert_eq!(summary(""), "Paired with Hue bridge ...");
        // Six characters, so no eight-character run of the bridge id is in it.
        let id = "001788FFFE0A1B2C";
        let line = summary(id);
        for window in id.as_bytes().windows(8) {
            let run = std::str::from_utf8(window).unwrap();
            assert!(!line.contains(run), "{line} carries {run}");
        }
    }

    #[test]
    fn every_line_this_flow_can_say_is_short_printable_and_written_here() {
        for line in [
            PRESS,
            NOT_IN_TIME,
            UNREACHABLE,
            REFUSED,
            UNREADABLE,
            CHANGED,
            NO_KEY,
        ] {
            assert!(!line.is_empty());
            assert!(line.len() <= couch_sdk::MAX_PAIR_TEXT, "{line}");
            assert!(!line.chars().any(char::is_control), "{line}");
        }
    }

    #[test]
    fn a_key_or_a_bridge_id_this_package_could_not_use_is_not_one_it_accepts() {
        assert!(usable("abc-123"));
        assert!(!usable(""));
        assert!(!usable("has space"));
        assert!(!usable(&"a".repeat(129)));
        assert!(identifies("001788FFFE0A1B2C"));
        assert!(!identifies(""));
        assert!(!identifies("00:17:88"));
    }

    #[test]
    fn a_flow_refuses_an_address_that_cannot_be_a_bridge_before_anything_opens() {
        for host in [
            "",
            "http://192.0.2.10",
            "https://a/path",
            "https://u:p@host",
        ] {
            assert_eq!(
                HueFlow::new(&HueSettings { host: host.into() })
                    .err()
                    .map(|error| error == Error::Configuration),
                Some(true),
                "{host}"
            );
        }
        let flow = HueFlow::new(&HueSettings {
            host: "192.0.2.10".into(),
        })
        .unwrap();
        // Nothing has been spoken to, so nothing has been pinned.
        assert!(flow.certificate().is_empty());
        assert_eq!(format!("{flow:?}"), "HueFlow { .. }");
    }

    #[test]
    fn an_address_nothing_answers_at_is_not_told_to_press_a_button_for_two_minutes() {
        let settings = HueSettings {
            host: "192.0.2.1".into(),
        };
        let mut flow = HueFlow::new(&settings).unwrap();
        // Nothing has ever answered: the first silence could be a slow
        // bridge, the second is an empty address.
        assert!(matches!(flow.unanswered(), Attempt::Slow));
        assert!(matches!(flow.unanswered(), Attempt::Unreachable));
        // A bridge that has shown its certificate is a bridge, however slow.
        let mut met = HueFlow::new(&settings).unwrap();
        met.pin.lock().unwrap().extend_from_slice(b"certificate");
        for _ in 0..10 {
            assert!(matches!(met.unanswered(), Attempt::Slow));
        }
    }

    #[test]
    fn a_cancelled_flow_holds_nothing() {
        let mut flow = HueFlow::new(&HueSettings {
            host: "192.0.2.10".into(),
        })
        .unwrap();
        flow.pin.lock().unwrap().extend_from_slice(&[1, 2, 3]);
        flow.cancel();
        assert!(flow.agent.is_none());
        assert!(flow.certificate().is_empty());
    }
}
