use clap::Parser;
use crypto::{digest::Digest, sha1::Sha1};
use freedesktop_icons::lookup;
use gdk_pixbuf::{glib::Bytes, Colorspace, Pixbuf};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env,
    error::Error,
    fs::{create_dir, read_dir, remove_file},
    path::Path,
    sync::Arc,
};
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
    theme: Option<String>,
    /// Location to save cached icons
    #[arg(short, long)]
    cache_dir: Option<String>,
}

fn lookup_icon(theme: &str, name: &str) -> String {
    match lookup(name)
        .with_cache()
        .with_theme("Adwaita")
        .with_theme(theme)
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
    cached_icons: &mut HashMap<String, i64>,
    theme: &str,
    cache_dir: &str,
) -> Result<(), Box<dyn Error>> {
    let body = msg.body::<Structure>();

    match msg.message_type() {
        MessageType::MethodCall => {
            if let Some(iface) = msg.interface() {
                // Push new notifications to the history stack
                if iface == InterfaceName::try_from("org.freedesktop.Notifications")? {
                    let body = body.unwrap();
                    let fields = body.fields();
                    let dict: Value = fields[6].clone();
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
                                    let mut hasher = Sha1::new();
                                    hasher.input(&bytes);
                                    let path = format!("{}/{}.png", cache_dir, hasher.result_str());
                                    if !Path::new(&path).exists() {
                                        Pixbuf::from_bytes(
                                            &bytes,
                                            Colorspace::Rgb,
                                            bool::try_from(img_fields[3].clone())?,
                                            i32::try_from(img_fields[4].clone())?,
                                            i32::try_from(img_fields[0].clone())?,
                                            i32::try_from(img_fields[1].clone())?,
                                            i32::try_from(img_fields[2].clone())?,
                                        )
                                        .savev(
                                            &path,
                                            "png",
                                            &[],
                                        )?;
                                    }
                                    *cached_icons.entry(path.clone()).or_insert(0) += 1;
                                    path
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
                } else if iface == InterfaceName::try_from("org.dunstproject.cmd0")? {
                    if let Some(member) = msg.member() {
                        if member == MemberName::try_from("NotificationRemoveFromHistory")? {
                            let body = body.unwrap();
                            let fields = body.fields();
                            let id = u32::try_from(fields[0].clone())?;
                            let icon_path =
                                history.iter().find(|x| x.id == id).unwrap().icon.clone();
                            history.remove(history.iter().position(|x| x.id == id).unwrap());

                            match cached_icons.get_mut(&icon_path) {
                                None => (),
                                Some(e) => {
                                    *e -= 1;
                                    if *e <= 0 {
                                        if remove_file(&icon_path).is_err() {
                                            eprintln!("Failed removing file {}", &icon_path);
                                        }
                                        cached_icons.remove(&icon_path);
                                    }
                                }
                            }
                            cached_icons.remove(&icon_path);

                            if let Err(err) = print_json(history) {
                                eprintln!("{}", err);
                            }
                        } else if member == MemberName::try_from("NotificationClearHistory")? {
                            buffer.drain(..);
                            history.drain(..);

                            for (icon, _) in cached_icons.iter_mut() {
                                if remove_file(icon).is_err() {
                                    eprintln!("Failed removing file {}", &icon);
                                }
                            }

                            cached_icons.drain();
                            println!("[]");
                        }
                    }
                }
            }
        }
        MessageType::MethodReturn => {
            let reply_serial = match msg.reply_serial() {
                Some(s) => s,
                None => return Ok(()),
            };
            let body = if body.is_ok() {
                body.unwrap()
            } else {
                return Ok(());
            };
            let fields = body.fields();
            match buffer.iter_mut().find(|x| x.serial == reply_serial) {
                Some(s) => s.id = u32::try_from(fields[0].clone()).unwrap(),
                None => return Ok(()),
            }
        }
        MessageType::Signal => {
            if let Some(member) = msg.member() {
                if member == MemberName::try_from("NotificationClosed")? {
                    let body = body.unwrap();
                    let fields = body.fields();
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
    let mut cached_icons: HashMap<String, i64> = HashMap::new();
    let theme = args.theme.unwrap_or(String::from("Adwaita"));
    let cache_dir = args.cache_dir.unwrap_or_else(|| {
        let path = format!(
            "{}/{}",
            env::var("XDG_CACHE_HOME").unwrap_or(String::from(".")),
            "disgustang"
        );
        if !Path::new(&path).exists() && create_dir(&path).is_err() {
            panic!("Failed creating cache directory {}", path);
        }
        path
    });

    let paths = read_dir(&cache_dir).unwrap();
    for p in paths {
        let p = p.unwrap().path();
        if remove_file(&p).is_err() {
            eprintln!("Failed removing file {}", p.display());
        }
    }

    let rules = [
        "type='method_call',interface='org.freedesktop.Notifications',member='Notify'",
        "type='method_return'",
        "type='signal',interface='org.freedesktop.Notifications',member='NotificationClosed'",
        "type='method_call',interface='org.dunstproject.cmd0',member='NotificationRemoveFromHistory'",
        "type='method_call',interface='org.dunstproject.cmd0',member='NotificationClearHistory'",
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
            &mut cached_icons,
            &theme,
            &cache_dir,
        ) {
            eprintln!("{}", err);
        }
    }

    Ok(())
}
