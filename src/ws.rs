use crate::{
    bridge::{join_parallel, replace_formatting, send_chat, send_command},
    config::{Config, Session},
    utils::{sys_check, sys_health_check, Clients, Result, WsClient},
};
use futures::{FutureExt, StreamExt};
use std::{
    env,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command, sync::mpsc};
use tokio_stream::wrappers::UnboundedReceiverStream;
use uuid::Uuid;
use warp::{
    ws::{Message, WebSocket},
    Reply,
};

lazy_static::lazy_static! {
    static ref CONFIG_PATH: String = {
        let path: Vec<String> = env::args().collect();
        path[0][..path[0].len() - 6].to_string()
    };
    static ref SESSIONS: Vec<Session> = {
        Config::load_sessions(CONFIG_PATH.to_string())
    };
    static ref RESTART_SCRIPT: Option<String> = {
        let config = Config::load_config(CONFIG_PATH.to_string());
        config.restart_script
    };
}

pub async fn client_connection(ws: WebSocket, clients: Clients) {
    println!("*info: establishing new client connection...");
    let (client_ws_sender, mut client_ws_rcv) = ws.split();
    let (client_sender, client_rcv) = mpsc::unbounded_channel();
    let client_rcv = UnboundedReceiverStream::new(client_rcv);
    tokio::task::spawn(client_rcv.forward(client_ws_sender).map(|result| {
        if let Err(e) = result {
            println!("*warn: \x1b[33merror sending websocket msg: {}\x1b[0m", e);
        }
    }));
    let uuid = Uuid::new_v4().to_simple().to_string();
    let new_client = WsClient {
        client_id: uuid.clone(),
        sender: Some(client_sender),
    };
    clients.lock().await.insert(uuid.clone(), new_client);
    while let Some(result) = client_ws_rcv.next().await {
        let msg = match result {
            Ok(msg) => msg,
            Err(e) => {
                println!(
                    "*warn: \x1b[33merror receiving message for id {}): {}\x1b[0m",
                    uuid.clone(),
                    e
                );
                break;
            }
        };
        client_msg(&uuid, msg, &clients).await;
    }
    clients.lock().await.remove(&uuid);
    println!("*info: {} disconnected", uuid);
}

async fn client_msg(client_id: &str, msg: Message, clients: &Clients) {
    let response = handle_response(msg).await;
    if response.is_none() {
        return;
    }

    let locked = clients.lock().await;
    match locked.get(client_id) {
        Some(v) => {
            if let Some(sender) = &v.sender {
                let _ = sender.send(Ok(Message::text(response.unwrap())));
            }
        }
        None => {}
    }
}

pub async fn ws_handler(ws: warp::ws::Ws, clients: Clients) -> Result<impl Reply> {
    Ok(ws.on_upgrade(move |socket| client_connection(socket, clients)))
}

fn get_cmd(msg: &str) -> Option<(&str, &str)> {
    let response = match msg.find(" ") {
        Some(v) => v,
        None => return None,
    };
    Some((&msg[..response], &msg[response + 1..]))
}

async fn handle_response(msg: Message) -> Option<String> {
    let message = match msg.to_str() {
        Ok(v) => v,
        Err(_) => return None,
    };

    let command_index = message.find(" ");

    // split the command into the first word if applicable
    let command = match command_index {
        Some(v) => &message[0..v],
        None => message,
    };
    let response = match command {
        "MSG" => {
            let (_, in_game_message) = match get_cmd(message) {
                Some(v) => v,
                None => return None,
            };
            let chat = replace_formatting(in_game_message);
            // TODO
            // replace with tmux json + cleanse input
            for server in &SESSIONS.to_vec() {
                send_command(
                    &server.name,
                    &format!(r#"tellraw @a {{ "text": "{}" }}"#, chat),
                );
            }
            return None;
        }
        "CMD" => {
            if command_index.is_none() {
                return Some("invalid command".to_string());
            }
            let (target, cmd) = match get_cmd(&message[command_index.unwrap() + 1..]) {
                Some(v) => v,
                None => return None,
            };
            println!("{}={}", target, cmd);
            send_command(target, cmd);
            return None;
        }
        "RESTART" => {
            let script_path = match RESTART_SCRIPT.to_owned() {
                Some(v) => v,
                None => return Some("no restart script found".to_string()),
            };
            let restart = Command::new("sh")
                .arg(script_path)
                .status()
                .await
                .expect("could not execute restart script");
            if restart.success() {
                return Some("restarting...".to_string());
            }
            return Some("failed to execute restart script".to_string());
        }
        "SHELL" => {
            let instructions: Vec<&str> =
                message[command_index.unwrap() + 1..].split(" ").collect();
            let command = instructions[0];
            let args = match instructions.len() {
                2.. => Some(&instructions[1..]),
                _ => None,
            };

            println!("*info: shell cmd {command}");
            if args.is_some() {
                let _ = Command::new(command).args(args.unwrap()).spawn();
                return None;
            }
            let _ = Command::new(command).spawn();
            return None;
        }
        "CHECK" => Some(sys_check()),
        "HEARTBEAT" => Some(sys_health_check().to_string()),
        "PING" => {
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis();
            //send_to_clients(clients, &format!("PONG {time}")).await;
            return Some(format!("PONG {time}"));
        }
        _ => None,
    };
    response
}
