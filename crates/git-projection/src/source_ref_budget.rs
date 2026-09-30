// SPDX-License-Identifier: Apache-2.0
//! Streaming bounds on an untrusted source ref advertisement (heddle#1905).
//!
//! Hosted import discovers a public source's branches and tags before it
//! contacts the destination, so the advertisement comes from a server the user
//! does not control. Sley buffers and parses a whole advertisement under
//! transport-scale ceilings (128 MiB for v0/v1, 2Mi frames / 256 MiB for v2
//! `ls-refs`), so the import's 512-ref admission bound would otherwise apply
//! only after every advertised name had been materialized.
//!
//! [`MeteredHttpClient`] wraps the HTTP client handed to Sley and meters every
//! response body while Sley reads it. It follows the pkt-line framing byte by
//! byte and fails the read as soon as the aggregate bytes, the record count, or
//! one record's length passes its import-scale bound, so the peer can make the
//! client read and hold at most [`MAX_SOURCE_ADVERTISEMENT_BYTES`].

use std::{
    fmt,
    io::{self, Read},
    sync::{Arc, Mutex, PoisonError},
};

use objects::object::thread_replication::git_import_graph::MAX_IMPORT_REFS;
use sley_transport::{HttpClient, HttpResponse};

/// Records (data pkt-lines) one discovery may read across all its responses.
///
/// A v0/v1 advertisement carries a peeled `^{}` line per annotated tag, and
/// every non-branch, non-tag ref the server holds (`refs/pull/*`, ...), which
/// import filters out but still has to read. Twice the import bound admits a
/// source whose every tag is annotated; the margin covers `HEAD`, the service
/// header, and protocol v2 capability lines.
pub const MAX_SOURCE_ADVERTISEMENT_RECORDS: u64 = 2 * MAX_IMPORT_REFS as u64 + 64;

/// Longest single record, pkt-line header included.
///
/// One ref per record, so this bounds each advertised ref name. It leaves room
/// for the object id, the v2 `symref-target:`/`peeled:` attributes, and the
/// capability list a v0 advertisement appends to its first ref.
pub const MAX_SOURCE_ADVERTISEMENT_RECORD_BYTES: u64 = 2048;

/// Aggregate response bytes one discovery may read.
///
/// Every admissible advertisement at realistic ref-name lengths is well under
/// this; it caps what a peer can make the client buffer when it pads records up
/// to [`MAX_SOURCE_ADVERTISEMENT_RECORD_BYTES`].
pub const MAX_SOURCE_ADVERTISEMENT_BYTES: u64 = 1024 * 1024;

/// Which advertisement bound a discovery hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefAdvertisementBound {
    /// [`MAX_SOURCE_ADVERTISEMENT_BYTES`].
    TotalBytes,
    /// [`MAX_SOURCE_ADVERTISEMENT_RECORDS`].
    Records,
    /// [`MAX_SOURCE_ADVERTISEMENT_RECORD_BYTES`].
    RecordBytes,
}

impl fmt::Display for RefAdvertisementBound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TotalBytes => "total size",
            Self::Records => "record count",
            Self::RecordBytes => "single record size",
        })
    }
}

/// A source advertisement was refused while it was being read.
///
/// `bytes_read` and `records_read` are what had been read when the read
/// stopped, which is at most one byte or one record past the bound, not the
/// size of what the peer was sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "source ref advertisement exceeds its {bound} limit of {limit} (stopped after {bytes_read} bytes and {records_read} records)"
)]
pub struct RefAdvertisementOverBudget {
    pub bound: RefAdvertisementBound,
    pub limit: u64,
    pub bytes_read: u64,
    pub records_read: u64,
}

#[derive(Debug, Default)]
struct Meter {
    bytes: u64,
    records: u64,
    exceeded: Option<RefAdvertisementOverBudget>,
}

impl Meter {
    fn exceed(&mut self, bound: RefAdvertisementBound, limit: u64) -> io::Error {
        let over = RefAdvertisementOverBudget {
            bound,
            limit,
            bytes_read: self.bytes,
            records_read: self.records,
        };
        self.exceeded = Some(over);
        io::Error::other(over)
    }
}

/// An [`HttpClient`] whose response bodies share one discovery budget.
///
/// Only `get` and `post` carry advertisements; the trait's other methods
/// default to them, so every body this client hands out is metered.
pub(crate) struct MeteredHttpClient<'a, C: HttpClient + ?Sized> {
    inner: &'a C,
    meter: Arc<Mutex<Meter>>,
}

impl<'a, C: HttpClient + ?Sized> MeteredHttpClient<'a, C> {
    pub(crate) fn new(inner: &'a C) -> Self {
        Self {
            inner,
            meter: Arc::default(),
        }
    }

    /// The bound that stopped a read, if any did.
    pub(crate) fn over_budget(&self) -> Option<RefAdvertisementOverBudget> {
        self.meter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .exceeded
    }

    fn metered(&self, response: HttpResponse) -> HttpResponse {
        let HttpResponse {
            status,
            content_type,
            content_length,
            content_range,
            body,
        } = response;
        HttpResponse {
            status,
            content_type,
            content_length,
            content_range,
            // `HttpResponse::body` is Sley's boxed reader type.
            body: Box::new(MeteredBody {
                inner: body,
                meter: Arc::clone(&self.meter),
                framing: PktFraming::default(),
            }),
        }
    }
}

impl<C: HttpClient + ?Sized> HttpClient for MeteredHttpClient<'_, C> {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> sley_core::Result<HttpResponse> {
        self.inner
            .get(url, headers)
            .map(|response| self.metered(response))
    }

    fn post(
        &self,
        url: &str,
        content_type: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> sley_core::Result<HttpResponse> {
        self.inner
            .post(url, content_type, headers, body)
            .map(|response| self.metered(response))
    }
}

struct MeteredBody {
    inner: Box<dyn Read + Send>,
    meter: Arc<Mutex<Meter>>,
    framing: PktFraming,
}

impl Read for MeteredBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut meter = self.meter.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(over) = meter.exceeded {
            return Err(io::Error::other(over));
        }
        // Never ask the peer for more than one byte past the aggregate budget.
        let headroom = MAX_SOURCE_ADVERTISEMENT_BYTES
            .saturating_sub(meter.bytes)
            .saturating_add(1);
        let window =
            usize::try_from(headroom).map_or(buf.len(), |headroom| headroom.min(buf.len()));
        let read = self.inner.read(&mut buf[..window])?;
        meter.bytes = meter.bytes.saturating_add(read as u64);
        if meter.bytes > MAX_SOURCE_ADVERTISEMENT_BYTES {
            return Err(meter.exceed(
                RefAdvertisementBound::TotalBytes,
                MAX_SOURCE_ADVERTISEMENT_BYTES,
            ));
        }
        self.framing.consume(&buf[..read], &mut meter)?;
        Ok(read)
    }
}

/// Position within the pkt-line stream of one response body.
#[derive(Default)]
struct PktFraming {
    header: [u8; 4],
    header_len: usize,
    payload_left: u64,
}

impl PktFraming {
    fn consume(&mut self, mut bytes: &[u8], meter: &mut Meter) -> io::Result<()> {
        while !bytes.is_empty() {
            if self.payload_left > 0 {
                let skip = usize::try_from(self.payload_left)
                    .map_or(bytes.len(), |left| left.min(bytes.len()));
                bytes = &bytes[skip..];
                self.payload_left -= skip as u64;
                continue;
            }
            let take = (self.header.len() - self.header_len).min(bytes.len());
            self.header[self.header_len..self.header_len + take].copy_from_slice(&bytes[..take]);
            self.header_len += take;
            bytes = &bytes[take..];
            if self.header_len < self.header.len() {
                continue;
            }
            self.header_len = 0;
            let length = self
                .header
                .iter()
                .try_fold(0_u64, |length, digit| {
                    char::from(*digit)
                        .to_digit(16)
                        .map(|digit| length * 16 + u64::from(digit))
                })
                // 3 is below the header's own size and is no control packet.
                .filter(|length| *length != 3)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "source ref advertisement has a malformed pkt-line length",
                    )
                })?;
            match length {
                // flush-pkt, delim-pkt, response-end-pkt.
                0..=2 => {}
                _ => {
                    meter.records += 1;
                    if meter.records > MAX_SOURCE_ADVERTISEMENT_RECORDS {
                        return Err(meter.exceed(
                            RefAdvertisementBound::Records,
                            MAX_SOURCE_ADVERTISEMENT_RECORDS,
                        ));
                    }
                    if length > MAX_SOURCE_ADVERTISEMENT_RECORD_BYTES {
                        return Err(meter.exceed(
                            RefAdvertisementBound::RecordBytes,
                            MAX_SOURCE_ADVERTISEMENT_RECORD_BYTES,
                        ));
                    }
                    self.payload_left = length - 4;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::git_core::{GitProjectionError, discover_git_source_refs_with_client};

    const URL: &str = "https://source.example.test/repo.git";
    const OID: &str = "1111111111111111111111111111111111111111";
    const PEELED: &str = "2222222222222222222222222222222222222222";
    /// Bytes the stub hands out per read, like one network segment.
    const SEGMENT: usize = 4096;

    fn pkt(payload: &str) -> Vec<u8> {
        format!("{:04x}{payload}", payload.len() + 4).into_bytes()
    }

    /// One advertised ref: its name and whether it is an annotated tag.
    type AdvertisedRef = (String, bool);
    type AdvertisedRefs = Box<dyn Iterator<Item = AdvertisedRef> + Send>;

    /// A smart-HTTP Git server that generates its advertisement lazily and
    /// counts every byte the client pulls from it.
    struct StubServer {
        protocol_v2: bool,
        refs: fn() -> AdvertisedRefs,
        pulled: Arc<AtomicU64>,
    }

    impl StubServer {
        fn new(protocol_v2: bool, refs: fn() -> AdvertisedRefs) -> Self {
            Self {
                protocol_v2,
                refs,
                pulled: Arc::default(),
            }
        }

        fn pulled(&self) -> u64 {
            self.pulled.load(Ordering::SeqCst)
        }

        fn response(
            &self,
            content_type: &str,
            lines: impl Iterator<Item = Vec<u8>> + Send + 'static,
        ) -> HttpResponse {
            HttpResponse {
                status: 200,
                content_type: Some(content_type.into()),
                content_length: None,
                content_range: None,
                body: Box::new(LazyBody {
                    lines,
                    current: Vec::new(),
                    offset: 0,
                    pulled: Arc::clone(&self.pulled),
                }),
            }
        }
    }

    impl HttpClient for StubServer {
        fn get(&self, _url: &str, _headers: &[(&str, &str)]) -> sley_core::Result<HttpResponse> {
            let service = [pkt("# service=git-upload-pack\n"), b"0000".to_vec()];
            let content_type = "application/x-git-upload-pack-advertisement";
            if self.protocol_v2 {
                let capabilities = [
                    "version 2\n",
                    "agent=git/2.45.0\n",
                    "ls-refs=unborn\n",
                    "fetch=shallow\n",
                    "object-format=sha1\n",
                ]
                .map(pkt);
                return Ok(self.response(
                    content_type,
                    service
                        .into_iter()
                        .chain(capabilities)
                        .chain([b"0000".to_vec()]),
                ));
            }
            let head = pkt(&format!(
                "{OID} HEAD\0multi_ack ofs-delta side-band-64k symref=HEAD:refs/heads/main agent=git/2.45.0\n"
            ));
            let refs = (self.refs)().flat_map(|(name, annotated)| {
                let mut lines = vec![pkt(&format!("{OID} {name}\n"))];
                if annotated {
                    lines.push(pkt(&format!("{PEELED} {name}^{{}}\n")));
                }
                lines
            });
            Ok(self.response(
                content_type,
                service
                    .into_iter()
                    .chain([head])
                    .chain(refs)
                    .chain([b"0000".to_vec()]),
            ))
        }

        fn post(
            &self,
            _url: &str,
            _content_type: &str,
            _headers: &[(&str, &str)],
            _body: &[u8],
        ) -> sley_core::Result<HttpResponse> {
            let head = pkt(&format!("{OID} HEAD symref-target:refs/heads/main\n"));
            let refs = (self.refs)().map(|(name, annotated)| {
                if annotated {
                    pkt(&format!("{OID} {name} peeled:{PEELED}\n"))
                } else {
                    pkt(&format!("{OID} {name}\n"))
                }
            });
            Ok(self.response(
                "application/x-git-upload-pack-result",
                [head].into_iter().chain(refs).chain([b"0000".to_vec()]),
            ))
        }
    }

    struct LazyBody<I> {
        lines: I,
        current: Vec<u8>,
        offset: usize,
        pulled: Arc<AtomicU64>,
    }

    impl<I: Iterator<Item = Vec<u8>>> Read for LazyBody<I> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            while self.offset == self.current.len() {
                match self.lines.next() {
                    Some(line) => {
                        self.current = line;
                        self.offset = 0;
                    }
                    None => return Ok(0),
                }
            }
            let read = buf.len().min(SEGMENT).min(self.current.len() - self.offset);
            buf[..read].copy_from_slice(&self.current[self.offset..self.offset + read]);
            self.offset += read;
            self.pulled.fetch_add(read as u64, Ordering::SeqCst);
            Ok(read)
        }
    }

    fn over_budget(server: &StubServer) -> RefAdvertisementOverBudget {
        match discover_git_source_refs_with_client(URL, Some(server)) {
            Err(GitProjectionError::SourceAdvertisementOverBudget(over)) => over,
            Err(other) => panic!("expected an over-budget refusal, got {other}"),
            Ok(refs) => panic!(
                "expected an over-budget refusal, discovered {} refs after pulling {} bytes",
                refs.len(),
                server.pulled()
            ),
        }
    }

    fn many_short_refs() -> AdvertisedRefs {
        Box::new((0..100_000).map(|index| (format!("refs/heads/branch-{index}"), false)))
    }

    fn one_huge_ref() -> AdvertisedRefs {
        Box::new(
            [
                ("refs/heads/main".to_string(), false),
                (format!("refs/heads/{}", "x".repeat(60_000)), false),
            ]
            .into_iter(),
        )
    }

    fn many_padded_refs() -> AdvertisedRefs {
        Box::new((0..100_000).map(|index| (format!("refs/heads/{index:0>1900}"), false)))
    }

    #[test]
    fn hundred_thousand_refs_stop_at_the_record_bound() {
        for protocol_v2 in [false, true] {
            let server = StubServer::new(protocol_v2, many_short_refs);
            let over = over_budget(&server);
            assert_eq!(
                over.bound,
                RefAdvertisementBound::Records,
                "v2={protocol_v2}"
            );
            assert_eq!(over.limit, MAX_SOURCE_ADVERTISEMENT_RECORDS);
            assert_eq!(over.records_read, MAX_SOURCE_ADVERTISEMENT_RECORDS + 1);
            // The whole advertisement is ~6.5 MB; the client stopped within one
            // segment of the record that crossed the bound.
            let per_record = 4 + OID.len() as u64 + " refs/heads/branch-99999\n".len() as u64;
            assert!(
                server.pulled() <= (MAX_SOURCE_ADVERTISEMENT_RECORDS + 1) * per_record + 1024,
                "v2={protocol_v2}: pulled {} bytes",
                server.pulled()
            );
        }
    }

    #[test]
    fn one_oversized_ref_name_stops_at_its_header() {
        for protocol_v2 in [false, true] {
            let server = StubServer::new(protocol_v2, one_huge_ref);
            let over = over_budget(&server);
            assert_eq!(
                over.bound,
                RefAdvertisementBound::RecordBytes,
                "v2={protocol_v2}"
            );
            assert_eq!(over.limit, MAX_SOURCE_ADVERTISEMENT_RECORD_BYTES);
            assert!(
                server.pulled() < 1024 + SEGMENT as u64,
                "v2={protocol_v2}: pulled {} bytes of a 60 KB record",
                server.pulled()
            );
        }
    }

    #[test]
    fn padded_refs_stop_at_the_byte_budget() {
        for protocol_v2 in [false, true] {
            let server = StubServer::new(protocol_v2, many_padded_refs);
            let over = over_budget(&server);
            assert_eq!(
                over.bound,
                RefAdvertisementBound::TotalBytes,
                "v2={protocol_v2}"
            );
            assert_eq!(over.limit, MAX_SOURCE_ADVERTISEMENT_BYTES);
            assert_eq!(over.bytes_read, MAX_SOURCE_ADVERTISEMENT_BYTES + 1);
            assert!(over.records_read < MAX_SOURCE_ADVERTISEMENT_RECORDS);
            assert_eq!(server.pulled(), MAX_SOURCE_ADVERTISEMENT_BYTES + 1);
        }
    }

    fn ordinary_source() -> AdvertisedRefs {
        Box::new(
            [
                ("refs/heads/feature/auth", false),
                ("refs/heads/main", false),
                ("refs/pull/12/head", false),
                ("refs/pull/12/merge", false),
                ("refs/remotes/origin/main", false),
                ("refs/tags/light", false),
                ("refs/tags/v1", true),
            ]
            .into_iter()
            .map(|(name, annotated)| (name.to_string(), annotated)),
        )
    }

    #[test]
    fn ordinary_source_discovers_only_branches_and_tags() {
        for protocol_v2 in [false, true] {
            let server = StubServer::new(protocol_v2, ordinary_source);
            let mut refs =
                discover_git_source_refs_with_client(URL, Some(&server)).expect("discovery");
            refs.sort();
            assert_eq!(
                refs,
                [
                    "refs/heads/feature/auth",
                    "refs/heads/main",
                    "refs/tags/light",
                    "refs/tags/v1"
                ],
                "v2={protocol_v2}"
            );
        }
    }

    /// The largest admissible source — every tag annotated, so a v0
    /// advertisement carries a peeled line for each — plus review refs the
    /// import filters out, all with long names, still fits every bound.
    fn largest_admissible_source() -> AdvertisedRefs {
        let pad = "p".repeat(200);
        let branches = (0..256).map(move |index| (format!("refs/heads/{pad}-{index}"), false));
        let pad = "p".repeat(200);
        let tags = (0..256).map(move |index| (format!("refs/tags/{pad}-{index}"), true));
        let reviews = (0..16).map(|index| (format!("refs/pull/{index}/head"), false));
        Box::new(branches.chain(tags).chain(reviews))
    }

    #[test]
    fn largest_admissible_source_fits_the_budget() {
        for protocol_v2 in [false, true] {
            let server = StubServer::new(protocol_v2, largest_admissible_source);
            let refs = discover_git_source_refs_with_client(URL, Some(&server)).expect("discovery");
            assert_eq!(refs.len(), MAX_IMPORT_REFS, "v2={protocol_v2}");
        }
    }
}
