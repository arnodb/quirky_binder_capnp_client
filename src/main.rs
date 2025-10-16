use std::{
    collections::BTreeMap,
    env::args,
    fmt::Write,
    sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use eframe::{
    egui,
    egui::{Align, Layout, TextWrapMode, ViewportCommand},
    glow,
};
use futures::{task::LocalSpawnExt, AsyncReadExt, AsyncWriteExt, FutureExt};
use quirky_binder_capnp::quirky_binder_capnp;
use resvg::tiny_skia;
use smol::{
    process::{Command, Stdio},
    Timer,
};
use teleop::{
    attach::unix_socket::connect, cancellation::CancellationToken,
    operate::capnp::client_connection,
};
use usvg::Tree;

const RUST_SVG: &str = include_str!("rust.svg");

pub fn node_name_to_dot_id(name: &str) -> String {
    format!("\"{name}\"")
}

pub async fn dot_to_svg(dot_source: &str) -> std::io::Result<String> {
    let mut child = Command::new("dot")
        .arg("-Tsvg")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(dot_source.as_bytes()).await?;
    }

    let output = child.output().await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let error_message = String::from_utf8_lossy(&output.stderr);
        Err(std::io::Error::other(format!(
            "Erreur lors de l'exécution de la commande dot : {error_message}"
        )))
    }
}

pub fn state_poller(
    sender: SyncSender<String>,
    ctx: egui::Context,
    cancellation_token: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut args = args();
    args.next();
    let pid: u32 = args
        .next()
        .unwrap_or_else(|| "PID missing".to_owned())
        .parse()?;

    let mut exec = futures::executor::LocalPool::new();
    let spawn = exec.spawner();

    exec.run_until(async move {
        let stream = connect(pid).await?;
        let (input, output) = stream.split();
        let (rpc_system, teleop) = client_connection(input, output).await;
        let rpc_disconnect = rpc_system.get_disconnector();

        spawn.spawn_local(async {
            if let Err(e) = rpc_system.await {
                eprintln!("Connection interrupted {e}");
            }
        })?;

        let mut req = teleop.service_request();
        req.get().set_name("state");
        let state = req.send().promise.await?;
        let state = state.get()?.get_service();
        let state: quirky_binder_capnp::state::Client = state.get_as()?;

        let graph = state.graph_request().send().promise.await?;
        let graph = graph.get()?.get_graph()?;

        let update_graph = async || -> Result<(), Box<dyn std::error::Error>> {
            let statuses = state.node_statuses_request().send().promise.await?;
            let statuses = statuses.get()?.get_statuses()?;
            let statuses = statuses
                .into_iter()
                .map(|s| Ok((s.get_node_name()?.to_str()?, s)))
                .collect::<capnp::Result<BTreeMap<&str, _>>>()?;

            let mut dot = String::new();

            writeln!(&mut dot, "digraph G {{")?;

            let nodes = graph.get_nodes()?;

            for node in nodes {
                writeln!(
                    &mut dot,
                    "{}",
                    node_name_to_dot_id(node.get_name()?.to_str()?)
                )?;
            }

            let edges = graph.get_edges()?;

            for edge in edges {
                let tail_name = edge.get_tail_name()?.to_str()?;
                let head_name = edge.get_head_name()?.to_str()?;

                write!(
                    &mut dot,
                    "{} -> {} [",
                    node_name_to_dot_id(tail_name),
                    node_name_to_dot_id(head_name)
                )?;

                let tail_index = edge.get_tail_index();
                let tail_counter = statuses
                    .get(tail_name)
                    .map(|s| capnp::Result::Ok(s.get_output_written()?.get(tail_index as _)))
                    .transpose()?;

                let head_index = edge.get_head_index();
                let head_counter = statuses
                    .get(head_name)
                    .map(|s| capnp::Result::Ok(s.get_input_read()?.get(head_index as _)))
                    .transpose()?;

                for (i, (attr, val)) in tail_counter
                    .map(|n| ("taillabel", n.to_string()))
                    .into_iter()
                    .chain(
                        head_counter
                            .map(|n| ("headlabel", n.to_string()))
                            .into_iter(),
                    )
                    .enumerate()
                {
                    if i > 0 {
                        write!(&mut dot, ", ")?;
                    } else {
                        writeln!(&mut dot)?;
                    }
                    writeln!(&mut dot, "{attr} = \"{val}\"",)?;
                }

                writeln!(&mut dot, "]")?;
            }
            writeln!(&mut dot, "}}")?;

            //println!("DOT: {dot}");

            let svg = dot_to_svg(&dot).await?;
            sender.send(svg).unwrap();

            ctx.request_repaint();

            Timer::after(Duration::from_millis(3000)).await;

            Ok(())
        };

        loop {
            let mut update = Box::pin(update_graph().fuse());
            let mut cancelled = cancellation_token.cancelled().fuse();
            futures::select! {
                res = update => {
                    let () = res?;
                }
                () = cancelled => {
                    break;
                }
            }
        }

        rpc_disconnect.await?;

        Timer::after(Duration::from_millis(3000)).await;

        Ok::<_, Box<dyn std::error::Error>>(())
    })?;

    exec.run();

    Ok(())
}

enum Content {
    Logo(Tree),
    Dot(Tree),
}

struct SvgViewer {
    content: Content,
    poller: Option<JoinHandle<Result<(), ()>>>,
    receiver: Receiver<String>,
    cancellation_token: CancellationToken,
    close_at: Option<Instant>,
}

impl SvgViewer {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_pixels_per_point(1.5);

        let (sender, receiver) = sync_channel(1);

        let cancellation_token = CancellationToken::new();

        let poller = std::thread::spawn({
            let ctx = cc.egui_ctx.clone();
            let cancellation_token = cancellation_token.clone();
            move || {
                let res =
                    state_poller(sender, ctx.clone(), cancellation_token.clone()).map_err(|err| {
                        eprintln!("Error in poller: {err}");
                    });
                ctx.request_repaint();
                res
            }
        });

        Self {
            content: Content::Logo(
                usvg::Tree::from_data(RUST_SVG.as_bytes(), &usvg::Options::default())
                    .expect("parse rust.svg"),
            ),
            poller: Some(poller),
            receiver,
            cancellation_token,
            close_at: None,
        }
    }
}

impl eframe::App for SvgViewer {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if let Some(at) = self.close_at {
            let now = Instant::now();
            if at < now {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(at - now);
            }
        }

        // https://github.com/emilk/egui/issues/5703
        if frame.info().cpu_usage.is_none() {
            return;
        }

        match self.receiver.try_recv() {
            Ok(svg) => {
                let mut options = usvg::Options::default();
                options.fontdb_mut().load_system_fonts();
                if let Ok(tree) = usvg::Tree::from_data(svg.as_bytes(), &options) {
                    self.content = Content::Dot(tree);
                }
                ctx.request_repaint();
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                let now = Instant::now();
                let close_at = match self.close_at {
                    Some(close_at) => close_at,
                    None => {
                        eprintln!("will close after 60s...");
                        let at = now + Duration::from_secs(60);
                        self.close_at = Some(at);
                        at
                    }
                };
                ctx.request_repaint_after(close_at - now);
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            match &self.content {
                Content::Logo(tree) => {
                    let pixmap_size = tree.size().to_int_size();
                    let available_size = ui.available_size();
                    let zoom = (available_size.x / pixmap_size.width() as f32)
                        .min(available_size.y / pixmap_size.height() as f32)
                        * 0.75;
                    let width = (pixmap_size.width() as f32 * zoom) as u32;
                    let height = (pixmap_size.height() as f32 * zoom) as u32;

                    if let Some(mut pixmap) = tiny_skia::Pixmap::new(width, height) {
                        resvg::render(
                            tree,
                            tiny_skia::Transform::from_scale(zoom, zoom),
                            &mut pixmap.as_mut(),
                        );

                        let image_texture = egui::ColorImage::from_rgba_unmultiplied(
                            [width as _, height as _],
                            pixmap.data(),
                        );

                        let handle =
                            ui.ctx()
                                .load_texture("svg-image", image_texture, Default::default());
                        let center_layout = Layout::top_down(Align::Center) // Sets Cross (Horizontal) Align to Center
                            .with_main_align(Align::Center) // Sets Main (Vertical) Align to Center
                            .with_main_justify(true); // Forces Main axis (Vertical) to fill space

                        ui.with_layout(center_layout, |ui| {
                            ui.add(egui::Image::new(&handle));
                        });
                    }
                }
                Content::Dot(tree) => {
                    let pixmap_size = tree.size().to_int_size();
                    let width = pixmap_size.width();
                    let height = pixmap_size.height();

                    if let Some(mut pixmap) = tiny_skia::Pixmap::new(width, height) {
                        resvg::render(tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());

                        let image_texture = egui::ColorImage::from_rgba_unmultiplied(
                            [width as _, height as _],
                            pixmap.data(),
                        );

                        let handle =
                            ui.ctx()
                                .load_texture("svg-image", image_texture, Default::default());
                        egui::ScrollArea::both().show(ui, |ui| {
                            ui.style_mut().wrap_mode = Some(TextWrapMode::Extend);
                            ui.add(egui::Image::new(&handle));
                        });
                    }
                }
            }
        });
    }

    fn on_exit(&mut self, _gl: Option<&glow::Context>) {
        self.cancellation_token.cancel();

        if let Some(poller) = self.poller.take() {
            match poller.join() {
                Ok(Ok(())) => {}
                Ok(Err(())) => {}
                Err(err) => {
                    eprintln!("Error joining poller: {err:?}");
                }
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Quirky Binder Cap'n Proto Client",
        native_options,
        Box::new(|cc| Ok(Box::new(SvgViewer::new(cc)))),
    )
}
