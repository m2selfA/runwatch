use crate::model::{HostCardView, RunRow, apply_host_run_usage, host_usage_summary};
use windui::prelude::*;

const PAGE_HORIZONTAL_CHROME: i32 = 80;
const CARD_GAP: i32 = 12;
const CARD_MIN_WIDTH: i32 = 280;
const CARD_MAX_WIDTH: i32 = 390;
const CARD_TEXT_CHROME: i32 = 126;
const CARD_BASE_HEIGHT: i32 = 168;
const ROUTE_LINE_HEIGHT: i32 = 18;

#[derive(Debug, Clone, PartialEq, Eq)]
struct HostWallLayout {
    hosts: Vec<HostCardView>,
    columns: usize,
    card_width: i32,
    card_height: i32,
    route_lines: usize,
}

#[derive(Clone)]
pub struct HostsViewState {
    hosts: Signal<Vec<HostCardView>>,
    last_rows: Signal<Vec<RunRow>>,
    status: Signal<String>,
    usage: Signal<String>,
    viewport_width: Signal<i32>,
    wall: Signal<Vec<HostWallLayout>>,
}

impl HostsViewState {
    pub fn new() -> Self {
        Self {
            hosts: signal(Vec::new()),
            last_rows: signal(Vec::new()),
            status: signal("Reading exact Host aliases from ~/.ssh/config...".into()),
            usage: signal("No live Runs are currently using an SSH/local Host.".into()),
            viewport_width: signal(1080),
            wall: signal(Vec::new()),
        }
    }

    pub fn apply_hosts(&self, result: Result<Vec<HostCardView>, String>) {
        match result {
            Ok(mut hosts) => {
                apply_host_run_usage(&mut hosts, &self.last_rows.get());
                self.status.set(if hosts.is_empty() {
                    "No exact Host aliases are declared in ~/.ssh/config.".into()
                } else {
                    format!(
                        "{} exact OpenSSH Host alias{} · read-only projection from ssh -G",
                        hosts.len(),
                        if hosts.len() == 1 { "" } else { "es" }
                    )
                });
                self.hosts.set(hosts);
            }
            Err(error) => {
                self.hosts.set(Vec::new());
                self.status.set(format!(
                    "Could not resolve OpenSSH Host aliases.\n{error}\nThis is a configuration error, not an empty Host list."
                ));
            }
        }
        self.rebuild_wall();
    }

    pub fn apply_run_usage(&self, rows: &[RunRow]) {
        self.last_rows.set(rows.to_vec());
        let usage = host_usage_summary(rows);
        if self.usage.get() != usage {
            self.usage.set(usage);
        }

        let mut hosts = self.hosts.get();
        let before = hosts.clone();
        apply_host_run_usage(&mut hosts, rows);
        if hosts != before {
            self.hosts.set(hosts);
            self.rebuild_wall();
        }
    }

    pub fn set_viewport_width(&self, width: i32) {
        if width > 0 && self.viewport_width.get() != width {
            self.viewport_width.set(width);
            self.rebuild_wall();
        }
    }

    pub fn build(&self) -> Element {
        let wall = Element::host_signal(self.wall, build_host_wall);
        Element::col()
            .fill()
            .padding(18)
            .spacing(10)
            .child(
                Element::label("SSH Hosts")
                    .font_size(22.0)
                    .fg(Color::hex(0x243746))
                    .width_match(),
            )
            .child(
                Element::label(
                    "Read-only OpenSSH inventory. Card layout reflows with the window; opening this page does not connect to every Host.",
                )
                .font_size(13.0)
                .fg(Color::hex(0x5B6B75))
                .width_match(),
            )
            .child(
                Element::label_signal(self.status)
                    .font_size(12.5)
                    .fg(Color::hex(0x5B6B75))
                    .width_match(),
            )
            .child(
                Element::card(
                    "Live Run usage",
                    Element::label_signal(self.usage)
                        .font_size(12.5)
                        .fg(Color::hex(0x2AA8A0))
                        .width_match(),
                )
                .width_match(),
            )
            .child(Element::scroll().fill().child(wall).weight(1.0))
    }

    fn rebuild_wall(&self) {
        let hosts = self.hosts.get();
        let next = if hosts.is_empty() {
            Vec::new()
        } else {
            vec![host_wall_layout(self.viewport_width.get(), hosts)]
        };
        if self.wall.get() != next {
            self.wall.set(next);
        }
    }
}

fn host_wall_layout(viewport_width: i32, hosts: Vec<HostCardView>) -> HostWallLayout {
    let available = (viewport_width - PAGE_HORIZONTAL_CHROME).max(220);
    let preferred = preferred_card_width(&hosts);
    let card_width = preferred.min(available);
    let columns = ((available + CARD_GAP) / (card_width + CARD_GAP)).max(1) as usize;
    let route_capacity = ((card_width - 44) / 7).max(18) as usize;
    let route_lines = hosts
        .iter()
        .map(|host| div_ceil(display_units(&host.route), route_capacity))
        .max()
        .unwrap_or(1)
        .clamp(1, 3);
    HostWallLayout {
        hosts,
        columns,
        card_width,
        card_height: CARD_BASE_HEIGHT + (route_lines.saturating_sub(1) as i32 * ROUTE_LINE_HEIGHT),
        route_lines,
    }
}

fn preferred_card_width(hosts: &[HostCardView]) -> i32 {
    let longest = hosts
        .iter()
        .flat_map(|host| {
            [
                display_units(&host.alias),
                display_units(&host.endpoint),
                display_units(&host.route),
                display_units(&host.usage_label()),
            ]
        })
        .max()
        .unwrap_or(20) as i32;
    (CARD_TEXT_CHROME + longest * 6).clamp(CARD_MIN_WIDTH, CARD_MAX_WIDTH)
}

fn display_units(value: &str) -> usize {
    value
        .chars()
        .map(|ch| if ch.is_ascii() { 1 } else { 2 })
        .sum()
}

fn div_ceil(value: usize, divisor: usize) -> usize {
    value.saturating_add(divisor.saturating_sub(1)) / divisor.max(1)
}

fn build_host_wall(layout: HostWallLayout) -> Element {
    let mut wall = Element::col().width_match().spacing(CARD_GAP);
    for chunk in layout.hosts.chunks(layout.columns.max(1)) {
        let mut row = Element::row()
            .width_match()
            .height(layout.card_height)
            .spacing(CARD_GAP)
            .cross(Align::Stretch);
        for host in chunk {
            row = row.child(host_card(host, &layout));
        }
        wall = wall.child(row);
    }
    wall
}

fn host_card(host: &HostCardView, layout: &HostWallLayout) -> Element {
    let usage = host.usage_label();
    let usage_color = if host.live_runs > 0 {
        Color::hex(0x2AA8A0)
    } else {
        Color::hex(0x5B6B75)
    };
    Element::card(
        host.alias.clone(),
        Element::col()
            .width_match()
            .spacing(6)
            .child(card_field(
                "Endpoint",
                &host.endpoint,
                1,
                Color::hex(0x243746),
            ))
            .child(card_field(
                "Route",
                &host.route,
                layout.route_lines,
                Color::hex(0x243746),
            ))
            .child(card_field("Usage", &usage, 1, usage_color)),
    )
    .width(layout.card_width)
    .height(layout.card_height)
}

fn card_field(label: &str, value: &str, lines: usize, color: Color) -> Element {
    Element::col()
        .width_match()
        .spacing(1)
        .child(
            Element::label(label)
                .font_size(10.5)
                .font_weight(600)
                .fg(Color::hex(0x7A8790))
                .width_match(),
        )
        .child(
            Element::label(value.to_string())
                .font_size(12.5)
                .fg(color)
                .max_lines(lines.max(1))
                .truncate(Truncate::End)
                .tooltip(value.to_string())
                .width_match(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(alias: &str, endpoint: &str, route: &str) -> HostCardView {
        HostCardView {
            alias: alias.into(),
            endpoint: endpoint.into(),
            route: route.into(),
            live_runs: 0,
        }
    }

    #[test]
    fn host_wall_reflows_columns_with_viewport_width() {
        let hosts = vec![
            host("gm00", "shark@gm00.example:22", "Direct"),
            host("gpu01", "user@gpu01.example:22", "Direct"),
            host("cap00", "inter@cap00.example:22", "Direct"),
            host("macos", "user@macos.example:22", "Direct"),
        ];
        let narrow = host_wall_layout(480, hosts.clone());
        let medium = host_wall_layout(900, hosts.clone());
        let wide = host_wall_layout(1440, hosts);
        assert_eq!(narrow.columns, 1);
        assert!(medium.columns >= 2);
        assert!(wide.columns > medium.columns);
    }

    #[test]
    fn longest_host_drives_one_shared_card_width_and_height() {
        let short = host("gm00", "u@gm00:22", "Direct");
        let long = host(
            "compute-gateway-long",
            "scientist@compute-gateway.internal.example:22022",
            "ProxyJump · bastion-alpha → bastion-beta → bastion-gamma → bastion-delta",
        );
        let short_only = host_wall_layout(1200, vec![short.clone()]);
        let layout = host_wall_layout(1200, vec![short, long]);
        assert!(layout.card_width > short_only.card_width);
        assert!(layout.card_width <= CARD_MAX_WIDTH);
        assert!(layout.card_height > short_only.card_height);
        assert!((1..=3).contains(&layout.route_lines));
    }
}
