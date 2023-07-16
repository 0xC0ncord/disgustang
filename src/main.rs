use base64::{engine::general_purpose, Engine as _};
use clap::Parser;
use freedesktop_icons::lookup;
use gdk_pixbuf::{glib::Bytes, Colorspace, Pixbuf};
use serde::{Deserialize, Serialize};
use std::{error::Error, sync::Arc};
use zbus::{
    export::futures_util::TryStreamExt,
    zvariant::{Structure, Value},
    Connection, Message, MessageStream, MessageType,
};
use zbus_names::{InterfaceName, MemberName};

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Notification {
    serial: u32,
    appname: String,
    summary: String,
    body: String,
    icon: String,
    icon_is_raw: bool,
    urgency: u8,
    id: u32,
}

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Length of the history. This should match dunst
    #[arg(short, long)]
    length: usize,
    /// An additional icon theme to use for icon lookups
    #[arg(short, long)]
    theme: String,
}

fn lookup_icon(theme: &str, name: &str) -> String {
    match lookup(name)
        .with_cache()
        .with_theme(theme)
        .with_theme("Adwaita")
        .find()
    {
        Some(s) => s.into_os_string().into_string().unwrap_or(String::new()),
        None => String::new(),
    }
}

fn handle_msg(
    msg: &mut Message,
    buffer: &mut Vec<Notification>,
    history: &mut Vec<Notification>,
    theme: &str,
) -> Result<(), Box<dyn Error>> {
    let Ok(body) = msg.body::<Structure>() else { return Ok(()) };
    let fields = body.fields();

    match msg.message_type() {
        // Push new notifications to the history stack
        MessageType::MethodCall => {
            if let Some(iface) = msg.interface() {
                if iface == InterfaceName::try_from("org.freedesktop.Notifications")? {
                    let dict: Value = fields[6].clone();
                    let mut icon_is_raw = false;
                    buffer.push(Notification {
                        serial: if let Some(s) = msg.primary_header().serial_num() {
                            *s
                        } else {
                            return Err("Failed getting serial number from message".into());
                        },
                        appname: String::try_from(fields[0].clone()).unwrap_or(String::new()),
                        summary: String::try_from(fields[3].clone()).unwrap_or(String::new()),
                        body: String::try_from(fields[4].clone()).unwrap_or(String::new()),
                        icon: if let Value::Dict(val) = &dict {
                            match val.get::<str, Structure>("image-data")? {
                                Some(i) => {
                                    let img_fields = i.fields();
                                    let bytes = if let Value::Array(a) = img_fields[6].clone() {
                                        Bytes::from(
                                            &(a.get()
                                                .iter()
                                                .map(|x| u8::try_from(x).unwrap())
                                                .collect::<Vec<u8>>()),
                                        )
                                    } else {
                                        Bytes::from_static(&[])
                                    };
                                    let mut img = String::from("data:image/png;base64,");
                                    img.push_str(
                                        &general_purpose::STANDARD.encode(
                                            Pixbuf::from_bytes(
                                                &bytes,
                                                Colorspace::Rgb,
                                                bool::try_from(img_fields[3].clone())?,
                                                i32::try_from(img_fields[4].clone())?,
                                                i32::try_from(img_fields[0].clone())?,
                                                i32::try_from(img_fields[1].clone())?,
                                                i32::try_from(img_fields[2].clone())?,
                                            )
                                            .save_to_bufferv("png", &[])?,
                                        ),
                                    );
                                    icon_is_raw = true;
                                    img
                                }
                                None => lookup_icon(
                                    theme,
                                    &String::try_from(fields[2].clone()).unwrap_or(String::new()),
                                ),
                            }
                        } else {
                            lookup_icon(
                                theme,
                                &String::try_from(fields[2].clone()).unwrap_or(String::new()),
                            )
                        },
                        icon_is_raw,
                        urgency: if let Value::Dict(val) = &dict {
                            match val.get("urgency")? {
                                Some(i) => *i,
                                None => 1,
                            }
                        } else {
                            1
                        },
                        id: 0,
                    });
                }
            }
        }
        MessageType::MethodReturn => {
            let reply_serial = match msg.reply_serial() {
                Some(s) => s,
                None => return Ok(()),
            };
            match buffer.iter_mut().find(|x| x.serial == reply_serial) {
                Some(s) => s.id = u32::try_from(fields[0].clone()).unwrap(),
                None => return Ok(()),
            }
        }
        MessageType::Signal => {
            if let Some(member) = msg.member() {
                if member == MemberName::try_from("NotificationClosed")? {
                    match buffer
                        .iter()
                        .find(|x| x.id == u32::try_from(fields[0].clone()).unwrap())
                    {
                        Some(s) => {
                            if history.len() == history.capacity() {
                                history.remove(0);
                            }
                            history.push(s.clone());
                            buffer.remove(buffer.iter().position(|x| x.id == s.id).unwrap());
                            if let Err(err) = print_json(history) {
                                eprintln!("{}", err);
                            }
                        }
                        None => return Ok(()),
                    }
                }
            }
        }
        _ => return Ok(()),
    }
    Ok(())
}

fn print_json(history: &Vec<Notification>) -> Result<(), Box<dyn Error>> {
    let mut hist = history.clone();
    hist.reverse();
    let json = match serde_json::to_string(&hist) {
        Ok(j) => j,
        Err(_) => return Err("Failed history to string conversion".into()),
    };
    println!("{}", json);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let mut buffer: Vec<Notification> = Vec::new();
    let mut history: Vec<Notification> = Vec::with_capacity(args.length);
    let theme = if args.theme.is_empty() {
        String::from("Adwaita")
    } else {
        args.theme
    };

    let rules = [
        "type='method_call',interface='org.freedesktop.Notifications',member='Notify'",
        "type='method_return'",
        "type='signal',interface='org.freedesktop.Notifications',member='NotificationClosed'",
        "type='method_call',interface='org.dunstproject.cmd0',member='NotificationRemoveFromHistory'",
        "type='method_call',interface='org.dunstproject.cmd0',member='NotificationClearHistory'"
    ];
    let connection = Connection::session().await?;
    connection
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus.Monitoring"),
            "BecomeMonitor",
            &(&rules as &[&str], 0u32),
        )
        .await?;

    let mut stream = MessageStream::from(connection);
    // Is this really the only way to get the inner value of the Arcs from this stream?
    while let Some(mut msg) = stream.try_next().await? {
        if let Err(err) = handle_msg(
            Arc::<zbus::Message>::make_mut(&mut msg),
            &mut buffer,
            &mut history,
            &theme,
        ) {
            eprintln!("{}", err);
        }
    }

    Ok(())
}
