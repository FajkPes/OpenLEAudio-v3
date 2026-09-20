//! Generic Media Control Service for the current Windows media session.
//! MCS 1.0: only confirmed Windows actions receive SUCCESS. Disconnect never
//! generates a player command. The server is scoped to the one active ACL link.
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub player: String,
    pub title: String,
    pub state: u8,
    pub duration: i32,
    pub position: i32,
    pub supported: u32,
}
#[derive(Clone, Debug)]
pub struct MediaCommand {
    pub id: u64,
    pub opcode: u8,
    pub position: Option<i32>,
}
struct Attribute {
    handle: u16,
    uuid: u16,
    value: Vec<u8>,
    read: bool,
    write: bool,
}
pub struct Server {
    attributes: Vec<Attribute>,
    subscriptions: BTreeMap<u16, bool>,
    notifications: VecDeque<Vec<u8>>,
    commands: VecDeque<MediaCommand>,
    pending: Option<(u64, u8, Instant)>,
    serial: u64,
    encrypted: bool,
    enabled: bool,
    snapshot: Snapshot,
}
impl Default for Server {
    fn default() -> Self {
        let mut s = Self {
            attributes: Vec::new(),
            subscriptions: BTreeMap::new(),
            notifications: VecDeque::new(),
            commands: VecDeque::new(),
            pending: None,
            serial: 0,
            encrypted: false,
            enabled: true,
            snapshot: Snapshot {
                player: "Windows".into(),
                duration: -1,
                ..Snapshot::default()
            },
        };
        s.attr(0x2800, 0x1800u16.to_le_bytes().to_vec(), true, false);
        s.characteristic(0x2A00, 2, b"OpenLEAudio".to_vec());
        s.characteristic(0x2A01, 2, vec![0, 0]);
        s.attr(0x2800, 0x1849u16.to_le_bytes().to_vec(), true, false);
        s.characteristic(0x2B93, 0x12, b"Windows".to_vec());
        s.characteristic(0x2B96, 0x10, vec![]);
        s.characteristic(0x2B97, 0x12, vec![]);
        s.characteristic(0x2B98, 0x12, (-1i32).to_le_bytes().to_vec());
        s.characteristic(0x2B99, 0x1E, 0i32.to_le_bytes().to_vec());
        s.characteristic(0x2BA3, 0x12, vec![0]);
        s.characteristic(0x2BA4, 0x1C, vec![]);
        s.characteristic(0x2BA5, 0x12, vec![0; 4]);
        s.characteristic(0x2BBA, 2, vec![1]);
        s
    }
}
impl Server {
    fn attr(&mut self, uuid: u16, value: Vec<u8>, read: bool, write: bool) -> u16 {
        let handle = self.attributes.len() as u16 + 1;
        self.attributes.push(Attribute {
            handle,
            uuid,
            value,
            read,
            write,
        });
        handle
    }
    fn characteristic(&mut self, uuid: u16, properties: u8, value: Vec<u8>) {
        let value_handle = self.attributes.len() as u16 + 2;
        let mut declaration = vec![properties];
        declaration.extend(value_handle.to_le_bytes());
        declaration.extend(uuid.to_le_bytes());
        self.attr(0x2803, declaration, true, false);
        let h = self.attr(uuid, value, properties & 2 != 0, properties & 0x0C != 0);
        if properties & 0x10 != 0 {
            self.attr(0x2902, vec![0, 0], true, true);
            self.subscriptions.insert(h, false);
        }
    }
    fn handle(&self, uuid: u16) -> u16 {
        self.attributes
            .iter()
            .find(|a| a.uuid == uuid)
            .unwrap()
            .handle
    }
    fn notify(&mut self, uuid: u16, value: Vec<u8>) {
        let h = self.handle(uuid);
        if self.encrypted && self.subscriptions.get(&h) == Some(&true) {
            let mut p = vec![0x1b];
            p.extend(h.to_le_bytes());
            p.extend(value);
            if self.notifications.len() < 64 {
                self.notifications.push_back(p);
            }
        }
    }
    fn value(&mut self, uuid: u16, value: Vec<u8>) {
        let a = self.attributes.iter_mut().find(|a| a.uuid == uuid).unwrap();
        if a.value != value {
            a.value = value.clone();
            self.notify(uuid, value);
        }
    }
    pub fn update(&mut self, snapshot: Snapshot) {
        let changed =
            self.snapshot.title != snapshot.title || self.snapshot.player != snapshot.player;
        self.value(
            0x2B93,
            snapshot
                .player
                .as_bytes()
                .iter()
                .copied()
                .take(512)
                .collect(),
        );
        self.value(
            0x2B97,
            snapshot
                .title
                .as_bytes()
                .iter()
                .copied()
                .take(512)
                .collect(),
        );
        self.value(0x2B98, snapshot.duration.to_le_bytes().to_vec());
        self.value(0x2B99, snapshot.position.to_le_bytes().to_vec());
        self.value(0x2BA3, vec![snapshot.state.min(3)]);
        self.value(
            0x2BA5,
            if self.enabled { snapshot.supported } else { 0 }
                .to_le_bytes()
                .to_vec(),
        );
        if changed {
            self.notify(0x2B96, vec![]);
        }
        self.snapshot = snapshot;
    }
    pub fn reset_link(&mut self) {
        self.encrypted = false;
        self.pending = None;
        self.commands.clear();
        self.notifications.clear();
        for v in self.subscriptions.values_mut() {
            *v = false;
        }
        for a in self.attributes.iter_mut().filter(|a| a.uuid == 0x2902) {
            a.value = vec![0, 0];
        }
    }
    pub fn finish(&mut self, id: u64, success: bool) {
        if let Some((pending, op, _)) = self.pending {
            if pending == id {
                self.pending = None;
                if op != 0 {
                    self.notify(0x2BA4, vec![op, if success { 1 } else { 4 }]);
                }
            }
        }
    }
    fn queue(&mut self, opcode: u8, position: Option<i32>) {
        let bit = match opcode {
            1 => 1,
            2 => 2,
            5 => 16,
            0x30 => 0x800,
            0x31 => 0x1000,
            0 => 0,
            _ => 0,
        };
        let error = if !self.enabled {
            Some(4)
        } else if opcode != 0 && (bit == 0 || self.snapshot.supported & bit == 0) {
            Some(2)
        } else if self.snapshot.state == 0 {
            Some(3)
        } else if self.pending.is_some() {
            Some(4)
        } else {
            None
        };
        if let Some(error) = error {
            if opcode != 0 {
                self.notify(0x2BA4, vec![opcode, error]);
            }
            return;
        }
        self.serial = self.serial.wrapping_add(1);
        self.commands.push_back(MediaCommand {
            id: self.serial,
            opcode,
            position,
        });
        self.pending = Some((self.serial, opcode, Instant::now()));
    }
    pub fn request(&mut self, p: &[u8], mtu: usize) -> Option<Vec<u8>> {
        let op = *p.first()?;
        if !matches!(
            op,
            0x02 | 0x04
                | 0x06
                | 0x08
                | 0x0a
                | 0x0c
                | 0x10
                | 0x12
                | 0x52
                | 0x16
                | 0x18
                | 0x0e
                | 0x20
        ) {
            return None;
        }
        let h = p
            .get(1..3)
            .map(|v| u16::from_le_bytes([v[0], v[1]]))
            .unwrap_or(0);
        let err = |code| vec![1, op, h as u8, (h >> 8) as u8, code];
        let mtu = mtu.clamp(23, 517);
        if op == 2 {
            return Some(if p.len() == 3 { vec![3, 5, 2] } else { err(4) });
        }
        if matches!(op, 4 | 6 | 8 | 0x10) {
            if p.len() < 5 {
                return Some(err(4));
            }
            let end = u16::from_le_bytes([p[3], p[4]]);
            if h == 0 || h > end {
                return Some(err(1));
            }
            let uuid = if p.len() == 21
                && matches!(op, 8 | 0x10)
                && p[5..17]
                    == [
                        0xfb, 0x34, 0x9b, 0x5f, 0x80, 0x00, 0x00, 0x80, 0x00, 0x10, 0x00, 0x00,
                    ]
                && p[19..21] == [0, 0]
            {
                Some(u16::from_le_bytes([p[17], p[18]]))
            } else if p.len() != 21 {
                p.get(5..7).map(|v| u16::from_le_bytes([v[0], v[1]]))
            } else {
                None
            };
            let mut rows = Vec::new();
            for (index, a) in self
                .attributes
                .iter()
                .enumerate()
                .filter(|(_, a)| a.handle >= h && a.handle <= end)
            {
                let mut row = Vec::new();
                row.extend(a.handle.to_le_bytes());
                match op {
                    4 => row.extend(a.uuid.to_le_bytes()),
                    0x10 if matches!(p.len(), 7 | 21)
                        && uuid == Some(0x2800)
                        && a.uuid == 0x2800 =>
                    {
                        let last = self.attributes[index + 1..]
                            .iter()
                            .find(|n| n.uuid == 0x2800)
                            .map(|n| n.handle - 1)
                            .unwrap_or(self.attributes.len() as u16);
                        row.extend(last.to_le_bytes());
                        row.extend(&a.value);
                    }
                    6 if p.len() == 9
                        && uuid == Some(0x2800)
                        && a.uuid == 0x2800
                        && a.value == p[7..] =>
                    {
                        let last = self.attributes[index + 1..]
                            .iter()
                            .find(|n| n.uuid == 0x2800)
                            .map(|n| n.handle - 1)
                            .unwrap_or(self.attributes.len() as u16);
                        row.extend(last.to_le_bytes());
                    }
                    8 if matches!(p.len(), 7 | 21) && uuid == Some(a.uuid) => {
                        if !a.read {
                            return Some(err(2));
                        }
                        if a.handle > 6 && a.uuid != 0x2803 && !self.encrypted {
                            return Some(err(0x0f));
                        }
                        row.extend(a.value.iter().take(mtu.saturating_sub(4).min(253)));
                    }
                    _ => continue,
                }
                if rows.first().is_some_and(|r: &Vec<u8>| r.len() != row.len()) {
                    break;
                }
                if 2 + rows.iter().map(Vec::len).sum::<usize>() + row.len() > mtu {
                    break;
                }
                rows.push(row);
            }
            if rows.is_empty() {
                return Some(err(0x0a));
            }
            let mut out = match op {
                4 => vec![5, 1],
                6 => vec![7],
                8 => vec![9, rows[0].len() as u8],
                _ => vec![0x11, 6],
            };
            for row in rows {
                out.extend(row);
            }
            return Some(out);
        }
        let Some(index) = self.attributes.iter().position(|a| a.handle == h) else {
            return Some(if op == 0x52 { vec![] } else { err(1) });
        };
        let uuid = self.attributes[index].uuid;
        if h > 6 && !self.encrypted {
            return Some(if op == 0x52 { vec![] } else { err(0x0f) });
        }
        if op == 0x0a || op == 0x0c {
            if !self.attributes[index].read {
                return Some(err(2));
            }
            let offset = if op == 0x0c {
                if p.len() != 5 {
                    return Some(err(4));
                }
                u16::from_le_bytes([p[3], p[4]]) as usize
            } else {
                if p.len() != 3 {
                    return Some(err(4));
                }
                0
            };
            let value = &self.attributes[index].value;
            if offset > value.len() {
                return Some(err(7));
            }
            let mut out = vec![if op == 0x0c { 0x0d } else { 0x0b }];
            out.extend(value[offset..].iter().take(mtu - 1));
            return Some(out);
        }
        if op == 0x12 || op == 0x52 {
            let mut error = None;
            if !self.attributes[index].write {
                error = Some(3);
            } else if uuid == 0x2902 {
                if p.len() != 5 || p[4] != 0 || p[3] > 1 {
                    error = Some(0x13);
                } else {
                    self.attributes[index].value = p[3..].to_vec();
                    self.subscriptions.insert(h - 1, p[3] == 1);
                }
            } else if uuid == 0x2BA4 {
                if p.len() != 4 {
                    error = Some(0x0d);
                } else {
                    self.queue(p[3], None);
                }
            } else if uuid == 0x2B99 {
                if p.len() != 7 {
                    error = Some(0x0d);
                } else {
                    self.queue(0, Some(i32::from_le_bytes(p[3..7].try_into().unwrap())));
                }
            } else {
                error = Some(3);
            }
            return Some(if op == 0x52 {
                vec![]
            } else if let Some(e) = error {
                err(e)
            } else {
                vec![0x13]
            });
        }
        Some(err(6))
    }
    pub fn notifications(&mut self, mtu: usize) -> Vec<Vec<u8>> {
        if let Some((id, _, at)) = self.pending {
            if at.elapsed() > Duration::from_secs(3) {
                self.finish(id, false);
            }
        }
        self.notifications
            .drain(..)
            .map(|mut p| {
                p.truncate(mtu.clamp(23, 517));
                p
            })
            .collect()
    }
}
fn server() -> &'static Mutex<Server> {
    static S: OnceLock<Mutex<Server>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Server::default()))
}
pub fn reset_link() {
    if let Ok(mut s) = server().lock() {
        s.reset_link();
    }
}
pub fn authorize() {
    if let Ok(mut s) = server().lock() {
        s.encrypted = true;
    }
}
pub fn update(snapshot: Snapshot) {
    if let Ok(mut s) = server().lock() {
        s.update(snapshot);
    }
}
pub fn enable(value: bool) {
    if let Ok(mut s) = server().lock() {
        s.enabled = value;
        let snap = s.snapshot.clone();
        s.update(snap);
    }
}
pub fn finish(id: u64, ok: bool) {
    if let Ok(mut s) = server().lock() {
        s.finish(id, ok);
    }
}
pub fn take_commands() -> Vec<MediaCommand> {
    server()
        .lock()
        .map(|mut s| s.commands.drain(..).collect())
        .unwrap_or_default()
}
pub fn serve(
    transport: &crate::transport::UsbTransport,
    handle: u16,
    pdu: Option<&[u8]>,
    mtu: usize,
) -> Result<bool, crate::transport::TransportError> {
    let (handled, mut packets) = if let Ok(mut s) = server().lock() {
        let response = pdu.and_then(|p| s.request(p, mtu));
        let handled = response.is_some();
        let mut packets = Vec::new();
        if let Some(p) = response {
            if !p.is_empty() {
                packets.push(p);
            }
        }
        packets.extend(s.notifications(mtu));
        (handled, packets)
    } else {
        (false, vec![])
    };
    for pdu in packets.drain(..) {
        for packet in crate::att::build_acl_packets(handle, crate::att::cid::ATT, &pdu, 27) {
            transport.send_acl(&packet)?;
        }
    }
    Ok(handled)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovers_gmcs_and_requires_encryption() {
        let mut s = Server::default();
        let p = s.request(&[0x10, 1, 0, 255, 255, 0, 0x28], 23).unwrap();
        assert!(p.windows(2).any(|v| v == [0x49, 0x18]));
        let h = s.handle(0x2BA3);
        assert_eq!(s.request(&[0x0a, h as u8, 0], 23).unwrap()[4], 0x0f);
        s.encrypted = true;
        assert_eq!(s.request(&[0x0a, h as u8, 0], 23), Some(vec![0x0b, 0]));
    }
    #[test]
    fn success_is_only_notified_after_windows_confirmation() {
        let mut s = Server::default();
        s.encrypted = true;
        s.update(Snapshot {
            state: 1,
            supported: 3,
            ..Snapshot::default()
        });
        let h = s.handle(0x2BA4);
        s.request(&[0x12, (h + 1) as u8, 0, 1, 0], 23);
        s.request(&[0x12, h as u8, 0, 2], 23);
        assert!(s.notifications(23).is_empty());
        let c = s.commands.pop_front().unwrap();
        assert_eq!(c.opcode, 2);
        s.finish(c.id, true);
        assert_eq!(s.notifications(23)[0], vec![0x1b, h as u8, 0, 2, 1]);
    }
    #[test]
    fn disconnect_never_queues_pause_and_cancels_old_results() {
        let mut s = Server::default();
        s.encrypted = true;
        s.update(Snapshot {
            state: 1,
            supported: 3,
            ..Snapshot::default()
        });
        s.queue(2, None);
        let id = s.pending.unwrap().0;
        s.reset_link();
        s.finish(id, true);
        assert!(s.commands.is_empty());
        assert!(s.notifications(23).is_empty());
    }
    #[test]
    fn att_responses_are_not_treated_as_requests() {
        let mut s = Server::default();
        for op in [1, 3, 5, 7, 9, 11, 13, 17, 19, 27, 29, 30] {
            assert!(s.request(&[op, 1, 0], 23).is_none());
        }
    }
}
