//! A tiny authoritative DNS server for tests: answers from a table the test
//! changes as it goes (UDP and TCP on one port, 127.0.0.1).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::sync::{Arc, Mutex};

use hickory_proto::op::{Message, OpCode, ResponseCode};
use hickory_proto::rr::rdata::{CNAME, MX, SRV, TXT};
use hickory_proto::rr::{Name, RData, Record, RecordType};

type Zone = Arc<Mutex<HashMap<(String, RecordType), Vec<RData>>>>;

pub struct Dns {
    pub addr: SocketAddr,
    zone: Zone,
}

fn answer(zone: &Zone, query: &[u8]) -> Option<Vec<u8>> {
    let request = Message::from_vec(query).ok()?;
    let mut response = Message::response(request.metadata.id, OpCode::Query);
    response.metadata.authoritative = true;
    let question = request.queries.first()?.clone();
    let name = question
        .name()
        .to_ascii()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let found = zone
        .lock()
        .unwrap()
        .get(&(name, question.query_type()))
        .cloned()
        .unwrap_or_default();
    if found.is_empty() {
        response.metadata.response_code = ResponseCode::NXDomain;
    }
    response.add_query(question.clone());
    for data in found {
        response.add_answer(Record::from_rdata(question.name().clone(), 60, data));
    }
    response.to_vec().ok()
}

impl Dns {
    pub fn start() -> Self {
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = udp.local_addr().unwrap();
        let tcp = TcpListener::bind(addr).unwrap();
        let zone: Zone = Arc::default();
        let z = Arc::clone(&zone);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok((n, from)) = udp.recv_from(&mut buf) {
                if let Some(reply) = answer(&z, &buf[..n]) {
                    let _ = udp.send_to(&reply, from);
                }
            }
        });
        let z = Arc::clone(&zone);
        std::thread::spawn(move || {
            for stream in tcp.incoming().flatten() {
                let z = Arc::clone(&z);
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut len = [0u8; 2];
                    while stream.read_exact(&mut len).is_ok() {
                        let mut query = vec![0u8; u16::from_be_bytes(len) as usize];
                        if stream.read_exact(&mut query).is_err() {
                            break;
                        }
                        let Some(reply) = answer(&z, &query) else {
                            break;
                        };
                        let _ = stream.write_all(&(reply.len() as u16).to_be_bytes());
                        let _ = stream.write_all(&reply);
                    }
                });
            }
        });
        Self { addr, zone }
    }

    fn set(&self, name: &str, kind: RecordType, data: Vec<RData>) {
        self.zone.lock().unwrap().insert(
            (name.trim_end_matches('.').to_ascii_lowercase(), kind),
            data,
        );
    }

    pub fn txt(&self, name: &str, values: &[&str]) {
        // Long values in 255-byte strings, as zone files have them.
        let data = values
            .iter()
            .map(|v| {
                RData::TXT(TXT::new(
                    v.as_bytes()
                        .chunks(255)
                        .map(|c| String::from_utf8_lossy(c).into_owned())
                        .collect(),
                ))
            })
            .collect();
        self.set(name, RecordType::TXT, data);
    }

    pub fn mx(&self, name: &str, preference: u16, host: &str) {
        self.set(
            name,
            RecordType::MX,
            vec![RData::MX(MX::new(
                preference,
                Name::from_ascii(host).unwrap(),
            ))],
        );
    }

    pub fn cname(&self, name: &str, host: &str) {
        self.set(
            name,
            RecordType::CNAME,
            vec![RData::CNAME(CNAME(Name::from_ascii(host).unwrap()))],
        );
    }

    pub fn srv(&self, name: &str, port: u16, host: &str) {
        self.set(
            name,
            RecordType::SRV,
            vec![RData::SRV(SRV::new(
                0,
                1,
                port,
                Name::from_ascii(host).unwrap(),
            ))],
        );
    }

    pub fn clear(&self, name: &str, kind: RecordType) {
        self.zone
            .lock()
            .unwrap()
            .remove(&(name.trim_end_matches('.').to_ascii_lowercase(), kind));
    }
}
