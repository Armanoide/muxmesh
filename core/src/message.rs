use crate::ack::Ack;
use crate::challenge_ok::ChallengeOk;
use crate::challenge::ChallengeWire;
use crate::command::Command;
use crate::directory::Directory;
use crate::hello::Hello;
use crate::report_state::ReportState;
use serde::{Deserialize, Serialize};

/// The wire protocol between hub and agents — one envelope, shared by both
/// binaries. Adjacently tagged: the discriminant (`type`) and the `payload`
/// are siblings, so a message can grow fields without touching the envelope.
///
/// On the wire every message is one JSON object per WS text frame:
///   {"type":"HELLO","payload":{"label":"macbook"}}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload", rename_all = "UPPERCASE")]
pub enum Message {
    /// agent -> hub: announce after upgrade.
    Hello(Hello),
    /// hub -> agent: handshake proof of hub identity.
    Challenge(ChallengeWire),
    /// agent -> hub: proof the agent verified the hub's challenge.
    ChallengeOk(ChallengeOk),
    /// hub -> agent: execute on local tmux (carries correlation id).
    Command(Command),
    /// agent -> hub: receipt for a Command (same id).
    Ack(Ack),
    /// agent -> hub: heartbeat.
    ReportState(ReportState),
    /// hub -> agent: the machine directory, on demand.
    Directory(Directory),
}

/// Encode a message to one wire line (JSON + trailing newline).
pub fn encode(msg: &Message) -> crate::error::Result<String> {
    let mut s = serde_json::to_string(msg)?;
    s.push('\n');
    Ok(s)
}

/// Decode one wire line. Unknown `type` => Err, never a silent default.
pub fn decode(line: &str) -> crate::error::Result<Message> {
    Ok(serde_json::from_str(line.trim_end())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Cmd;
    use crate::report_state::BarStatus;

    #[test]
    fn envelope_is_adjacently_tagged() {
        let m = Message::Hello(Hello { label: "macbook".into() });
        assert_eq!(
            encode(&m).unwrap(),
            "{\"type\":\"HELLO\",\"payload\":{\"label\":\"macbook\"}}\n"
        );
    }

    #[test]
    fn every_message_round_trips() {
        let cases = vec![
            Message::Hello(Hello { label: "macbook".into() }),
            Message::Challenge(ChallengeWire { nonce: "aa".into(), sig: "bb".into() }),
            Message::ChallengeOk(ChallengeOk { nonce: "aa".into() }),
            Message::Command(Command { id: 42, cmd: Cmd::BarHide, target: "main:1".into() }),
            Message::Ack(Ack { id: 42, ok: true }),
            Message::ReportState(ReportState { seen: 1730000000, status: BarStatus::On }),
            Message::Directory(Directory { me: "macbook".into(), armed: vec!["node1".into()] }),
        ];
        for m in cases {
            assert_eq!(decode(&encode(&m).unwrap()).unwrap(), m);
        }
    }

    #[test]
    fn unknown_type_is_rejected_not_defaulted() {
        assert!(decode("{\"type\":\"LAUNCH_MISSILES\",\"payload\":{}}\n").is_err());
    }

    #[test]
    fn unknown_cmd_is_rejected_not_defaulted() {
        let bad = "{\"type\":\"COMMAND\",\"payload\":{\"id\":1,\"cmd\":\"REBOOT\",\"target\":\"x\"}}\n";
        assert!(decode(bad).is_err());
    }

    #[test]
    fn payload_field_addition_does_not_touch_envelope() {
        // The envelope shape is fixed {"type","payload"}; a payload can gain
        // fields without the envelope changing. Simulated here by decoding a
        // HELLO whose payload carries an extra field: serde ignores unknown
        // fields by default, so the envelope still routes correctly.
        let line = "{\"type\":\"HELLO\",\"payload\":{\"label\":\"x\",\"extra\":1}}";
        assert_eq!(decode(line).unwrap(), Message::Hello(Hello { label: "x".into() }));
    }

    #[test]
    fn ack_correlates_to_command_by_id() {
        let cmd = Message::Command(Command { id: 7, cmd: Cmd::BarShow, target: "m".into() });
        let ack = Message::Ack(Ack { id: 7, ok: true });
        if let (Message::Command(c), Message::Ack(a)) = (&cmd, &ack) {
            assert_eq!(c.id, a.id);
        } else {
            panic!("wrong message types");
        }
    }
}
