//! Root-only bounded Unix-socket server and fixed system-manager job entrypoints.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use nix::unistd::User;
use tessera_network::handoff::{
    Command as HandoffCommand, ControlRequest, Product, Request as HandoffRequest,
};
use tessera_network::{SOCKET, engine::Engine, read_line};

fn main() -> ExitCode {
    // Netplan's generated networkd files must retain their intended group
    // readability. Every helper-owned private file/directory sets its own mode.
    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o022));
    if !nix::unistd::geteuid().is_root() {
        eprintln!("tessera-network must run as root");
        return ExitCode::FAILURE;
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    let engine = Engine::default();
    let handoff = match args.as_slice() {
        [op, action, owner] if op == "handoff" && action == "prepare" && owner == "ndimon" => {
            Some(HandoffCommand::HandoffPrepare {
                receiver: Product::Ndimon,
            })
        }
        [op, action, id] if op == "handoff" && action == "cancel" => {
            Some(HandoffCommand::HandoffCancel { id: id.clone() })
        }
        [op, action, product, id]
            if op == "handoff" && action == "import" && product == "ndimon" =>
        {
            Some(HandoffCommand::HandoffImport {
                product: Product::Ndimon,
                id: id.clone(),
            })
        }
        [op, action, product, id, impact]
            if op == "handoff" && action == "commit" && product == "ndimon" =>
        {
            Some(HandoffCommand::HandoffCommit {
                product: Product::Ndimon,
                id: id.clone(),
                expect_impact: (impact != "--yes").then(|| impact.clone()),
                confirm: impact == "--yes",
            })
        }
        [op, action] if op == "handoff" && action == "status" => {
            Some(HandoffCommand::HandoffStatus {})
        }
        _ => None,
    };
    if let Some(command) = handoff {
        let reply =
            engine.handle_control(ControlRequest::Handoff(HandoffRequest { api: 2, command }));
        println!("{reply}");
        return if reply.get("error").is_some_and(|v| !v.is_null()) {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        };
    }
    // Protected JSON stdin supports the same typed requests as control.sock,
    // including Keep/Revert without placing a secret token in process arguments.
    if args == ["request"] {
        return match read_line::<ControlRequest>(std::io::stdin().lock()) {
            Ok(req) => {
                let reply = engine.handle_control(req);
                println!("{reply}");
                if reply.get("error").is_some_and(|v| !v.is_null()) {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                }
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        };
    }
    let result = match args.as_slice() {
        [op] if op == "serve" => serve(engine),
        [op] if op == "boot-recover" => engine.boot(),
        [op] if op == "recover-expired" => engine.recover(None, true),
        [op, token] if op == "apply" => engine.apply(token),
        [op, token] if op == "recover" => engine.recover(Some(token), false),
        [op] if op == "accept-current" => engine
            .accept_current(false)
            .map(|report| print!("{report}")),
        [op, yes] if op == "accept-current" && yes == "--yes" => engine
            .accept_current(true)
            .map(|report| print!("{report}")),
        _ => Err(
            "usage: tessera-network serve|boot-recover|recover-expired|apply JOB|recover JOB|accept-current [--yes]|handoff prepare ndimon|handoff cancel ID|handoff import ndimon ID|handoff commit ndimon ID IMPACT|handoff status|request"
                .into(),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tessera-network: {e}");
            ExitCode::FAILURE
        }
    }
}

fn serve(engine: Engine) -> Result<(), String> {
    engine.store.prepare()?;
    let uid = User::from_name("tessera-web")
        .map_err(|e| e.to_string())?
        .ok_or("Missing tessera-web user")?
        .uid
        .as_raw();
    match std::fs::remove_file(SOCKET) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.to_string()),
    }
    let listener = UnixListener::bind(SOCKET).map_err(|e| e.to_string())?;
    std::fs::set_permissions(SOCKET, std::fs::Permissions::from_mode(0o660))
        .map_err(|e| e.to_string())?;
    let clients = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = stream.map_err(|e| e.to_string())?;
        let peer = getsockopt(&stream, PeerCredentials).map_err(|e| e.to_string())?;
        if (peer.uid() != uid && peer.uid() != 0) || clients.load(Ordering::Relaxed) >= 16 {
            continue;
        }
        clients.fetch_add(1, Ordering::Relaxed);
        let count = Arc::clone(&clients);
        let engine = engine.clone();
        std::thread::spawn(move || {
            if let Err(e) = respond(&engine, stream) {
                eprintln!("tessera-network request: {e}");
            }
            count.fetch_sub(1, Ordering::Relaxed);
        });
    }
    Ok(())
}

fn respond(engine: &Engine, mut stream: UnixStream) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let request: ControlRequest = read_line(&stream)?;
    let mut data =
        serde_json::to_vec(&engine.handle_control(request)).map_err(|e| e.to_string())?;
    data.push(b'\n');
    stream.write_all(&data).map_err(|e| e.to_string())
}
