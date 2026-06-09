// SPDX-License-Identifier: Apache-2.0
//! Native fetch/push transport for bare mirrors (#577 P3, F4 track A).
//!
//! Fetch orchestration delegates to [`sley_remote`] (HTTP v1/v2, SSH shallow
//! fetch, local in-process). Push/receive-pack ref probes keep the substrate
//! wire helpers until heddle's custom push planner can call `sley_remote::push`
//! directly.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use sley_config::GitConfig;
use sley_core::{GitError, ObjectFormat, ObjectId};
use sley_remote::{
    FetchOptions, FetchRequest, FetchServices, FetchSource, NoCredentials, RemoteTransportKind,
    SilentProgress, TransportCapabilities, fetch, fetch_source_for_url, transport_kind_for_url,
};
use sley_fetch::install_upload_pack_raw_response;
use sley_odb::FileObjectDatabase;
use sley_protocol::{
    FetchRefUpdate, GitService, ReceivePackCommand, ReceivePackCommandStatus,
    ReceivePackFeatures, ReceivePackPushRequest, ReceivePackPushRequestOptions,
    ReceivePackReportStatus, ReceivePackUnpackStatus, RefAdvertisement,
    UploadPackNegotiationRequest, UploadPackRequest, UploadPackRawPackfileResponse,
    build_receive_pack_push_request, parse_receive_pack_features, parse_refspec,
    parse_upload_pack_features, plan_fetch_ref_updates,
    read_receive_pack_report_status, read_ref_advertisement_set,
    read_upload_pack_raw_packfile_response, smart_http_advertisement_content_type,
    smart_http_info_refs_path, smart_http_rpc_path, smart_http_rpc_request_content_type,
    smart_http_rpc_result_content_type, write_receive_pack_push_request,
    write_upload_pack_negotiation_request, write_upload_pack_request,
};
use sley_refs::{FileRefStore, RefTarget};
use sley_transport::{
    RemoteTransport, RemoteUrl, ServiceDiscoveryPayload, ServiceRequest, SshCommandVariant,
    parse_remote_url, read_service_discovery_response, ssh_process_command, write_service_request,
};

use crate::repo::GitRepo;
use crate::{GitSubstrateError, Result};

const USER_AGENT: &str = "heddle-git-substrate";

/// Transport capabilities of the linked `sley-remote` build.
pub fn transport_capabilities() -> TransportCapabilities {
    TransportCapabilities::current()
}

/// Whether [`fetch_bare_mirror`] can handle `url` natively.
///
/// When `depth` is `Some`, shallow fetch must be supported for the transport class.
pub fn supports_native_fetch(url: &str) -> bool {
    supports_native_fetch_with_depth(url, None)
}

/// Like [`supports_native_fetch`] but accounts for shallow clone/fetch depth.
pub fn supports_native_fetch_with_depth(url: &str, depth: Option<u32>) -> bool {
    if let Ok(parsed) = parse_remote_url(url) {
        // `git://` is not SSH; keep the substrate git-daemon wire path (no shallow yet).
        if parsed.transport == RemoteTransport::Git {
            return depth.is_none();
        }
    }
    let Ok(Some(kind)) = transport_kind_for_url(url) else {
        return false;
    };
    let caps = TransportCapabilities::current();
    if depth.is_some() && !caps.supports_shallow(kind) {
        return false;
    }
    match kind {
        RemoteTransportKind::Http => caps.http_fetch,
        RemoteTransportKind::Ssh => caps.ssh_fetch,
        // Heddle handles `file://` clones via `copy_reachable_objects`; bare-mirror
        // fetch over the network transport stack is HTTP/SSH/git:// only.
        RemoteTransportKind::Local | RemoteTransportKind::Bundle => false,
    }
}

/// Whether [`push_receive_pack`] / [`receive_pack_ref_map`] can handle `url`
/// natively.
pub fn supports_native_push(url: &str) -> bool {
    if let Ok(parsed) = parse_remote_url(url) {
        // `git://` is not SSH; substrate keeps the git-daemon wire path.
        if parsed.transport == RemoteTransport::Git {
            return true;
        }
    }
    let Ok(Some(kind)) = transport_kind_for_url(url) else {
        return false;
    };
    match kind {
        RemoteTransportKind::Local | RemoteTransportKind::Bundle => false,
        _ => TransportCapabilities::current().supports_native_push(kind),
    }
}

fn matches_native_transport(transport: RemoteTransport) -> bool {
    matches!(
        transport,
        RemoteTransport::Ssh | RemoteTransport::Http | RemoteTransport::Https | RemoteTransport::Git
    )
}

/// A single receive-pack ref update to push over the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushCommand {
    pub name: String,
    pub old_id: ObjectId,
    pub new_id: ObjectId,
}

/// Fetch ref tips and objects from `url` into bare `dest_git_dir`.
pub fn fetch_bare_mirror(
    dest_git_dir: &Path,
    format: ObjectFormat,
    url: &str,
    refspecs: &[String],
    fetch_tags: bool,
    depth: Option<u32>,
    reflog_message: &str,
) -> Result<Option<String>> {
    if !supports_native_fetch_with_depth(url, depth) {
        return Err(GitSubstrateError::Git(GitError::Unsupported(format!(
            "native fetch is not available for {url}"
        ))));
    }

    let parsed = parse_remote_url(url).map_err(GitSubstrateError::from)?;
    if parsed.transport == RemoteTransport::Git {
        return fetch_bare_mirror_via_git_protocol(
            dest_git_dir,
            format,
            &parsed,
            url,
            refspecs,
            fetch_tags,
            reflog_message,
        );
    }

    let relative_base = dest_git_dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(dest_git_dir);
    let source = fetch_source_for_url(url, relative_base).map_err(GitSubstrateError::from)?;
    if matches!(source, FetchSource::Local { .. }) {
        return Err(GitSubstrateError::Git(GitError::Unsupported(
            "native bare-mirror fetch does not serve local paths; use copy_reachable_objects"
                .into(),
        )));
    }

    let config = GitConfig::default();
    // `dry_run: true` still downloads packfiles into the ODB; it only skips writing
    // refs on the remote side. We apply ref updates ourselves via
    // `apply_fetch_ref_updates` so a crash between fetch and ref-apply leaves
    // orphaned objects but cannot corrupt ref state.
    let options = FetchOptions {
        quiet: true,
        auto_follow_tags: false,
        fetch_all_tags: fetch_tags,
        prune: false,
        dry_run: true,
        append: false,
        write_fetch_head: false,
        tag_option_explicit: true,
        prune_option_explicit: true,
        depth,
        merge_src: None,
    };
    let mut credentials = NoCredentials;
    let mut progress = SilentProgress;
    let outcome = fetch(
        FetchRequest {
            git_dir: dest_git_dir,
            format,
            config: &config,
            remote_name: "heddle-mirror",
            source: &source,
            refspecs,
            options: &options,
        },
        FetchServices {
            credentials: &mut credentials,
            progress: &mut progress,
        },
    )
    .map_err(GitSubstrateError::from)?;

    apply_fetch_ref_updates(dest_git_dir, format, &outcome.ref_updates, reflog_message)?;
    Ok(default_branch_from_head_symref(outcome.head_symref.as_deref()))
}

fn default_branch_from_head_symref(head_symref: Option<&str>) -> Option<String> {
    head_symref
        .and_then(|target| target.strip_prefix("refs/heads/"))
        .filter(|branch| !branch.is_empty())
        .map(str::to_string)
}

fn fetch_bare_mirror_via_git_protocol(
    dest_git_dir: &Path,
    format: ObjectFormat,
    parsed: &RemoteUrl,
    url: &str,
    refspecs: &[String],
    fetch_tags: bool,
    reflog_message: &str,
) -> Result<Option<String>> {
    let mut effective_refspecs = refspecs.to_vec();
    if fetch_tags {
        effective_refspecs.push("+refs/tags/*:refs/tags/*".to_string());
    }
    let parsed_refspecs = effective_refspecs
        .iter()
        .map(|refspec| parse_refspec(refspec))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(GitSubstrateError::from)?;
    let advertisements = git_upload_pack_advertisements(parsed, format, url)?;
    let mut updates =
        plan_fetch_ref_updates(&advertisements, &parsed_refspecs, fetch_tags)
            .map_err(GitSubstrateError::from)?;
    if fetch_tags {
        for update in &mut updates {
            if update.src.starts_with("refs/tags/") && update.dst.as_deref() == Some(&update.src) {
                update.not_for_merge = true;
            }
        }
    }
    let wants = updates
        .iter()
        .map(|update| update.oid.clone())
        .collect::<Vec<_>>();
    install_fetch_pack_via_git_protocol(dest_git_dir, format, parsed, url, wants)?;
    apply_fetch_ref_updates(dest_git_dir, format, &updates, reflog_message)?;
    Ok(default_branch_from_advertisements(&advertisements))
}

/// Read the remote's receive-pack ref advertisement as a name → oid map.
pub fn receive_pack_ref_map(url: &str, format: ObjectFormat) -> Result<HashMap<String, ObjectId>> {
    let parsed = parse_remote_url(url).map_err(GitSubstrateError::from)?;
    if !matches_native_transport(parsed.transport) {
        return Err(GitSubstrateError::Git(GitError::Unsupported(
            "native push only supports SSH, HTTP(S), and git:// remotes".into(),
        )));
    }
    let advertisements = receive_pack_advertisements(&parsed, format, url)?;
    Ok(advertisements
        .into_iter()
        .map(|advertisement| (advertisement.name, advertisement.oid))
        .collect())
}

/// Push `commands` and `packfile` to `url` via receive-pack.
pub fn push_receive_pack(
    url: &str,
    format: ObjectFormat,
    commands: &[PushCommand],
    packfile: &[u8],
) -> Result<()> {
    if commands.is_empty() {
        return Ok(());
    }
    let parsed = parse_remote_url(url).map_err(GitSubstrateError::from)?;
    if !matches_native_transport(parsed.transport) {
        return Err(GitSubstrateError::Git(GitError::Unsupported(
            "native push only supports SSH, HTTP(S), and git:// remotes".into(),
        )));
    }

    let advertisements = receive_pack_advertisements(&parsed, format, url)?;
    let features = receive_pack_features_from_advertisements(&advertisements)?;
    if !features.report_status && !features.report_status_v2 {
        return Err(GitSubstrateError::Git(GitError::Command(format!(
            "remote {url} does not support report-status; refusing to push without server acknowledgement"
        ))));
    }

    let receive_commands = commands
        .iter()
        .map(|command| ReceivePackCommand {
            old_id: command.old_id.clone(),
            new_id: command.new_id.clone(),
            name: command.name.clone(),
        })
        .collect::<Vec<_>>();
    let request = build_receive_pack_push_request(
        &features,
        receive_commands.clone(),
        packfile.to_vec(),
        ReceivePackPushRequestOptions {
            report_status: features.report_status,
            report_status_v2: features.report_status_v2,
            ..ReceivePackPushRequestOptions::default()
        },
    )
    .map_err(GitSubstrateError::from)?;

    let report = execute_receive_pack_push(&parsed, url, &request)?;
    validate_receive_pack_report(&report, &receive_commands, url)
}

fn default_branch_from_advertisements(advertisements: &[RefAdvertisement]) -> Option<String> {
    for advertisement in advertisements {
        let Ok(features) = parse_upload_pack_features(&advertisement.capabilities) else {
            continue;
        };
        for symref in &features.symrefs {
            if let Some(target) = symref.strip_prefix("HEAD:") {
                return target
                    .strip_prefix("refs/heads/")
                    .map(|branch| branch.to_string());
            }
        }
    }
    None
}

fn receive_pack_features_from_advertisements(
    advertisements: &[RefAdvertisement],
) -> Result<ReceivePackFeatures> {
    let Some(first) = advertisements.first() else {
        return Ok(ReceivePackFeatures::default());
    };
    parse_receive_pack_features(&first.capabilities).map_err(GitSubstrateError::from)
}

fn receive_pack_advertisements(
    parsed: &RemoteUrl,
    format: ObjectFormat,
    remote_url: &str,
) -> Result<Vec<RefAdvertisement>> {
    match parsed.transport {
        RemoteTransport::Ssh => ssh_receive_pack_advertisements(remote_url, format),
        RemoteTransport::Http | RemoteTransport::Https => {
            smart_http_receive_pack_advertisements(parsed, format, remote_url)
        }
        RemoteTransport::Git => git_receive_pack_advertisements(parsed, format, remote_url),
        _ => Err(GitSubstrateError::Git(GitError::Unsupported(
            "native push only supports SSH, HTTP(S), and git:// remotes".into(),
        ))),
    }
}

fn execute_receive_pack_push(
    parsed: &RemoteUrl,
    remote_url: &str,
    request: &ReceivePackPushRequest,
) -> Result<ReceivePackReportStatus> {
    match parsed.transport {
        RemoteTransport::Ssh => ssh_push_receive_pack(remote_url, request),
        RemoteTransport::Http | RemoteTransport::Https => {
            smart_http_push_receive_pack(parsed, remote_url, request)
        }
        RemoteTransport::Git => git_push_receive_pack(parsed, remote_url, request),
        _ => Err(GitSubstrateError::Git(GitError::Unsupported(
            "native push only supports SSH, HTTP(S), and git:// remotes".into(),
        ))),
    }
}

fn ssh_receive_pack_advertisements(
    remote_url: &str,
    format: ObjectFormat,
) -> Result<Vec<RefAdvertisement>> {
    let parsed = parse_remote_url(remote_url).map_err(GitSubstrateError::from)?;
    let ssh = ssh_process_command(
        &parsed,
        GitService::ReceivePack,
        "ssh",
        SshCommandVariant::OpenSsh,
    )
    .map_err(GitSubstrateError::from)?;
    let output = Command::new(&ssh.program)
        .args(&ssh.args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| GitSubstrateError::Git(GitError::from(err)))?;
    let mut stdout = output.stdout.as_slice();
    let set = match read_ref_advertisement_set(format, &mut stdout) {
        Ok(set) => set,
        Err(_) if !output.status.success() => {
            return Err(GitSubstrateError::Git(GitError::Command(format!(
                "ssh receive-pack failed for {remote_url}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))));
        }
        Err(err) => return Err(GitSubstrateError::from(err)),
    };
    Ok(set.refs)
}

fn ssh_push_receive_pack(
    remote_url: &str,
    request: &ReceivePackPushRequest,
) -> Result<ReceivePackReportStatus> {
    let parsed = parse_remote_url(remote_url).map_err(GitSubstrateError::from)?;
    let format = request
        .commands
        .commands
        .first()
        .map(|command| command.new_id.format())
        .unwrap_or(ObjectFormat::Sha1);
    let ssh = ssh_process_command(
        &parsed,
        GitService::ReceivePack,
        "ssh",
        SshCommandVariant::OpenSsh,
    )
    .map_err(GitSubstrateError::from)?;
    let mut child = Command::new(&ssh.program)
        .args(&ssh.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| GitSubstrateError::Git(GitError::from(err)))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| GitSubstrateError::Git(GitError::Command("ssh stdout not piped".into())))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| GitSubstrateError::Git(GitError::Command("ssh stdin not piped".into())))?;

    read_ref_advertisement_set(format, &mut stdout).map_err(GitSubstrateError::from)?;
    write_receive_pack_push_request(&mut stdin, request).map_err(GitSubstrateError::from)?;
    drop(stdin);

    let report = read_receive_pack_report_status(&mut stdout).map_err(GitSubstrateError::from)?;
    let output = child
        .wait_with_output()
        .map_err(|err| GitSubstrateError::Git(GitError::from(err)))?;
    if !output.status.success() {
        return Err(GitSubstrateError::Git(GitError::Command(format!(
            "ssh receive-pack failed for {remote_url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))));
    }
    Ok(report)
}

fn smart_http_receive_pack_advertisements(
    parsed: &RemoteUrl,
    format: ObjectFormat,
    remote_url: &str,
) -> Result<Vec<RefAdvertisement>> {
    let body = smart_http_info_refs_get(parsed, GitService::ReceivePack, remote_url)?;
    advertised_refs_from_discovery_body(format, &body, remote_url)
}

fn smart_http_push_receive_pack(
    parsed: &RemoteUrl,
    remote_url: &str,
    request: &ReceivePackPushRequest,
) -> Result<ReceivePackReportStatus> {
    let mut body = Vec::new();
    write_receive_pack_push_request(&mut body, request).map_err(GitSubstrateError::from)?;
    let response_body =
        smart_http_rpc_post(parsed, GitService::ReceivePack, remote_url, &body)?;
    let mut reader = response_body.as_slice();
    read_receive_pack_report_status(&mut reader).map_err(GitSubstrateError::from)
}

fn git_upload_pack_advertisements(
    parsed: &RemoteUrl,
    format: ObjectFormat,
    remote_url: &str,
) -> Result<Vec<RefAdvertisement>> {
    let mut stream = git_protocol_connect(parsed, remote_url)?;
    write_service_request(&mut stream, &git_service_request(parsed, GitService::UploadPack))
        .map_err(GitSubstrateError::from)?;
    let set = read_ref_advertisement_set(format, &mut stream).map_err(GitSubstrateError::from)?;
    Ok(set.refs)
}

fn git_receive_pack_advertisements(
    parsed: &RemoteUrl,
    format: ObjectFormat,
    remote_url: &str,
) -> Result<Vec<RefAdvertisement>> {
    let mut stream = git_protocol_connect(parsed, remote_url)?;
    write_service_request(&mut stream, &git_service_request(parsed, GitService::ReceivePack))
        .map_err(GitSubstrateError::from)?;
    let set = read_ref_advertisement_set(format, &mut stream).map_err(GitSubstrateError::from)?;
    Ok(set.refs)
}

fn install_fetch_pack_via_git_protocol(
    git_dir: &Path,
    format: ObjectFormat,
    parsed: &RemoteUrl,
    remote_url: &str,
    wants: Vec<ObjectId>,
) -> Result<()> {
    if skip_fetch_pack_install(git_dir, format, &wants)? {
        return Ok(());
    }

    let request = UploadPackRequest {
        wants,
        ..UploadPackRequest::default()
    };
    let haves = local_have_oids(git_dir)?;
    let mut stream = git_protocol_connect(parsed, remote_url)?;
    write_service_request(&mut stream, &git_service_request(parsed, GitService::UploadPack))
        .map_err(GitSubstrateError::from)?;
    read_ref_advertisement_set(format, &mut stream).map_err(GitSubstrateError::from)?;
    write_upload_pack_session(&mut stream, &request, &haves).map_err(GitSubstrateError::from)?;

    let response =
        read_upload_pack_raw_packfile_response(format, &mut stream).map_err(GitSubstrateError::from)?;
    install_upload_pack_response(git_dir, format, &response)
}

fn git_push_receive_pack(
    parsed: &RemoteUrl,
    remote_url: &str,
    request: &ReceivePackPushRequest,
) -> Result<ReceivePackReportStatus> {
    let format = request
        .commands
        .commands
        .first()
        .map(|command| command.new_id.format())
        .unwrap_or(ObjectFormat::Sha1);
    let mut stream = git_protocol_connect(parsed, remote_url)?;
    write_service_request(&mut stream, &git_service_request(parsed, GitService::ReceivePack))
        .map_err(GitSubstrateError::from)?;
    read_ref_advertisement_set(format, &mut stream).map_err(GitSubstrateError::from)?;
    write_receive_pack_push_request(&mut stream, request).map_err(GitSubstrateError::from)?;
    read_receive_pack_report_status(&mut stream).map_err(GitSubstrateError::from)
}

fn advertised_refs_from_discovery_body(
    format: ObjectFormat,
    body: &[u8],
    remote_url: &str,
) -> Result<Vec<RefAdvertisement>> {
    let mut reader = body;
    let discovery = read_service_discovery_response(format, &mut reader)
        .map_err(GitSubstrateError::from)?;
    match discovery.payload {
        ServiceDiscoveryPayload::AdvertisedRefs(set) => Ok(set.refs),
        ServiceDiscoveryPayload::ProtocolV2(_) => Err(GitSubstrateError::Git(GitError::Unsupported(
            format!(
                "native fetch for {remote_url} requires git protocol v1; remote advertised protocol v2"
            ),
        ))),
    }
}

fn smart_http_info_refs_get(
    parsed: &RemoteUrl,
    service: GitService,
    remote_url: &str,
) -> Result<Vec<u8>> {
    let url = format!(
        "{}{}",
        http_base_url(parsed)?,
        smart_http_info_refs_path(&parsed.path, service).map_err(GitSubstrateError::from)?
    );
    let accept = smart_http_advertisement_content_type(service).map_err(GitSubstrateError::from)?;
    let parsed = parsed.clone();
    let remote_url = remote_url.to_string();
    run_blocking_http(move || {
        let response = apply_http_auth(
            http_client()?
                .get(&url)
                .header("Accept", accept)
                .header("User-Agent", USER_AGENT),
            &parsed,
        )
        .send()
        .map_err(http_error)?;
        let status = response.status();
        let body = response.bytes().map_err(http_error)?.to_vec();
        if !status.is_success() {
            return Err(http_status_error(&remote_url, "info/refs", status.as_u16(), &body));
        }
        Ok(body)
    })
}

fn smart_http_rpc_post(
    parsed: &RemoteUrl,
    service: GitService,
    remote_url: &str,
    body: &[u8],
) -> Result<Vec<u8>> {
    let url = format!(
        "{}{}",
        http_base_url(parsed)?,
        smart_http_rpc_path(&parsed.path, service).map_err(GitSubstrateError::from)?
    );
    let content_type =
        smart_http_rpc_request_content_type(service).map_err(GitSubstrateError::from)?;
    let accept = smart_http_rpc_result_content_type(service).map_err(GitSubstrateError::from)?;
    let parsed = parsed.clone();
    let remote_url = remote_url.to_string();
    let request_body = body.to_vec();
    run_blocking_http(move || {
        let response = apply_http_auth(
            http_client()?
                .post(&url)
                .header("Content-Type", content_type)
                .header("Accept", accept)
                .header("User-Agent", USER_AGENT)
                .body(request_body),
            &parsed,
        )
        .send()
        .map_err(http_error)?;
        let status = response.status();
        let body = response.bytes().map_err(http_error)?.to_vec();
        if !status.is_success() {
            return Err(http_status_error(
                &remote_url,
                service.as_str(),
                status.as_u16(),
                &body,
            ));
        }
        Ok(body)
    })
}

fn http_base_url(parsed: &RemoteUrl) -> Result<String> {
    let scheme = match parsed.transport {
        RemoteTransport::Http => "http",
        RemoteTransport::Https => "https",
        _ => {
            return Err(GitSubstrateError::Git(GitError::InvalidFormat(
                "HTTP base URL requires an HTTP(S) remote".into(),
            )));
        }
    };
    let host = parsed.host.as_deref().ok_or_else(|| {
        GitSubstrateError::Git(GitError::InvalidFormat(
            "HTTP remote is missing a host".into(),
        ))
    })?;
    let mut url = format!("{scheme}://");
    if let Some(user) = &parsed.user {
        let username = user.split(':').next().unwrap_or(user.as_str());
        if !username.is_empty() {
            url.push_str(username);
            url.push('@');
        }
    }
    url.push_str(host);
    if let Some(port) = parsed.port {
        url.push(':');
        url.push_str(&port.to_string());
    }
    Ok(url)
}

fn git_protocol_connect(parsed: &RemoteUrl, remote_url: &str) -> Result<TcpStream> {
    let host = parsed.host.as_deref().ok_or_else(|| {
        GitSubstrateError::Git(GitError::InvalidFormat(
            "git:// remote is missing a host".into(),
        ))
    })?;
    let port = parsed.port.unwrap_or(9418);
    let addr = format!("{host}:{port}");
    TcpStream::connect(&addr).map_err(|err| {
        GitSubstrateError::Git(GitError::Command(format!(
            "git:// connect failed for {remote_url} ({addr}): {err}"
        )))
    })
}

fn git_service_request(parsed: &RemoteUrl, service: GitService) -> ServiceRequest {
    ServiceRequest {
        service,
        path: parsed.path.clone(),
        host: parsed.host.clone(),
        parameters: Vec::new(),
        protocol: None,
        extra_parameters: Vec::new(),
    }
}

fn write_upload_pack_session(
    writer: &mut impl Write,
    request: &UploadPackRequest,
    haves: &[ObjectId],
) -> std::result::Result<(), GitError> {
    write_upload_pack_request(writer, Some(request))?;
    write_upload_pack_negotiation_request(
        writer,
        &UploadPackNegotiationRequest {
            haves: haves.to_vec(),
            done: true,
        },
    )
}

fn skip_fetch_pack_install(
    git_dir: &Path,
    format: ObjectFormat,
    wants: &[ObjectId],
) -> Result<bool> {
    if wants.is_empty() {
        return Ok(true);
    }
    let local_db = FileObjectDatabase::from_git_dir(git_dir, format);
    Ok(wants
        .iter()
        .map(|want| local_db.contains(want))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(GitSubstrateError::from)?
        .into_iter()
        .all(|contains| contains))
}

fn install_upload_pack_response(
    git_dir: &Path,
    format: ObjectFormat,
    response: &UploadPackRawPackfileResponse,
) -> Result<()> {
    let local_db = FileObjectDatabase::from_git_dir(git_dir, format);
    install_upload_pack_raw_response(response, &local_db).map_err(GitSubstrateError::from)?;
    Ok(())
}

fn validate_receive_pack_report(
    report: &ReceivePackReportStatus,
    commands: &[ReceivePackCommand],
    url: &str,
) -> Result<()> {
    match &report.unpack {
        ReceivePackUnpackStatus::Ok => {}
        ReceivePackUnpackStatus::Error(message) => {
            return Err(GitSubstrateError::Git(GitError::Command(format!(
                "push pack rejected by {url}: {message}"
            ))));
        }
    }

    let mut acknowledged = HashSet::new();
    for status in &report.commands {
        match status {
            ReceivePackCommandStatus::Ok { name } => {
                acknowledged.insert(name.clone());
            }
            ReceivePackCommandStatus::Ng { name, message } => {
                return Err(GitSubstrateError::Git(GitError::Command(format!(
                    "push rejected by {url} for {name}: {message}"
                ))));
            }
        }
    }

    for command in commands {
        if !acknowledged.contains(&command.name) {
            return Err(GitSubstrateError::Git(GitError::Command(format!(
                "push to {url} did not acknowledge ref {}",
                command.name
            ))));
        }
    }
    Ok(())
}

fn http_credentials(parsed: &RemoteUrl) -> Option<(&str, &str)> {
    let user = parsed.user.as_deref()?;
    let (username, password) = user.split_once(':')?;
    if username.is_empty() || password.is_empty() {
        return None;
    }
    Some((username, password))
}

fn apply_http_auth(
    builder: reqwest::blocking::RequestBuilder,
    parsed: &RemoteUrl,
) -> reqwest::blocking::RequestBuilder {
    match http_credentials(parsed) {
        Some((username, password)) => builder.basic_auth(username, Some(password)),
        None => builder,
    }
}

/// Run blocking HTTP on a worker thread so reqwest's internal runtime is not
/// dropped from inside the CLI's Tokio executor.
fn run_blocking_http<T: Send + 'static>(
    task: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    std::thread::spawn(task)
        .join()
        .map_err(|_| {
            GitSubstrateError::Git(GitError::Command(
                "native HTTP worker thread panicked".into(),
            ))
        })?
}

fn http_client() -> Result<reqwest::blocking::Client> {
    static RUSTLS: OnceLock<()> = OnceLock::new();
    let _ = RUSTLS.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|err| {
            GitSubstrateError::Git(GitError::Command(format!(
                "failed to build HTTP client: {err}"
            )))
        })
}

fn http_error(err: reqwest::Error) -> GitSubstrateError {
    let message = if err.is_connect() {
        format!("failed to connect for native HTTP transport: {err}")
    } else {
        format!("HTTP request failed: {err}")
    };
    GitSubstrateError::Git(GitError::Command(message))
}

fn http_status_error(remote_url: &str, phase: &str, status: u16, body: &[u8]) -> GitSubstrateError {
    let snippet = String::from_utf8_lossy(body);
    let trimmed = snippet.trim();
    let detail = if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {trimmed}")
    };
    GitSubstrateError::Git(GitError::Command(format!(
        "HTTP {phase} failed for {remote_url} with status {status}{detail}"
    )))
}

fn local_have_oids(git_dir: &Path) -> Result<Vec<ObjectId>> {
    let repo = GitRepo::open(git_dir)?;
    let mut seen = HashSet::new();
    let mut haves = Vec::new();
    if let Some(oid) = repo.read_ref_oid("HEAD")? {
        if seen.insert(oid.clone()) {
            haves.push(oid);
        }
    }
    for reference in repo.list_refs()? {
        if let sley_refs::RefTarget::Direct(oid) = reference.target
            && seen.insert(oid.clone())
        {
            haves.push(oid);
        }
    }
    Ok(haves)
}

fn apply_fetch_ref_updates(
    git_dir: &Path,
    format: ObjectFormat,
    updates: &[FetchRefUpdate],
    reflog_message: &str,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let mut tx = store.transaction();
    for update in updates {
        let Some(dst) = update.dst.as_ref() else {
            continue;
        };
        let old_oid = match store.read_ref(dst).map_err(GitSubstrateError::from)? {
            Some(RefTarget::Direct(oid)) => oid,
            _ => zero_oid(format),
        };
        tx.update(sley_refs::RefUpdate {
            name: dst.clone(),
            expected: None,
            new: RefTarget::Direct(update.oid.clone()),
            reflog: Some(sley_refs::ReflogEntry {
                old_oid,
                new_oid: update.oid.clone(),
                committer: crate::refs::bridge_reflog_committer(),
                message: reflog_message.as_bytes().to_vec(),
            }),
        });
    }
    tx.commit().map_err(GitSubstrateError::from)
}

fn zero_oid(format: ObjectFormat) -> ObjectId {
    ObjectId::from_hex(format, &"0".repeat(format.hex_len())).expect("zero oid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_native_fetch_accepts_network_transports() {
        assert!(supports_native_fetch("git@github.com:org/repo.git"));
        assert!(supports_native_fetch("https://github.com/org/repo.git"));
        assert!(supports_native_fetch("http://example.com/repo.git"));
        assert!(supports_native_fetch("git://example.com/repo.git"));
        assert!(!supports_native_fetch("file:///tmp/repo.git"));
    }

    #[test]
    fn supports_native_push_accepts_network_transports() {
        assert!(supports_native_push("git@github.com:org/repo.git"));
        assert!(supports_native_push("https://github.com/org/repo.git"));
        assert!(supports_native_push("http://example.com/repo.git"));
        assert!(supports_native_push("git://example.com/repo.git"));
        assert!(!supports_native_push("file:///tmp/repo.git"));
    }

    #[test]
    fn default_branch_from_head_symref_reads_trunk() {
        assert_eq!(
            default_branch_from_head_symref(Some("refs/heads/trunk")).as_deref(),
            Some("trunk")
        );
    }
}