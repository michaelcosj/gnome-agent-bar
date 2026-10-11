import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Pango from 'gi://Pango';
import St from 'gi://St';

import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

import * as fmt from './format.js';

const LEVELS = ['normal', 'warning', 'critical'];
const TICK_SECONDS = 60;
const DIM_OPACITY = 160;

// Ring (top bar) or bar (menu) gauge. Color comes from CSS per level.
const Meter = GObject.registerClass(
class AgentBarMeter extends St.DrawingArea {
    _init(shape, styleClass) {
        super._init({style_class: styleClass, y_align: Clutter.ActorAlign.CENTER});
        this._shape = shape;
        this._pct = 0;
    }

    setValue(pct, lvl) {
        this._pct = Math.min(100, Math.max(0, pct ?? 0));
        setLevel(this, lvl);
        this.queue_repaint();
    }

    vfunc_repaint() {
        const cr = this.get_context();
        const [w, h] = this.get_surface_size();
        const c = this.get_theme_node().get_foreground_color();
        const [r, g, b] = [c.red / 255, c.green / 255, c.blue / 255];
        const frac = this._pct / 100;

        if (this._shape === 'ring') {
            const lw = Math.max(2, Math.round(w / 6));
            const radius = Math.min(w, h) / 2 - lw / 2;
            const start = -Math.PI / 2;
            cr.setLineWidth(lw);
            cr.setSourceRGBA(r, g, b, 0.3);
            cr.arc(w / 2, h / 2, radius, 0, 2 * Math.PI);
            cr.stroke();
            if (frac > 0) {
                cr.setSourceRGBA(r, g, b, 1);
                cr.arc(w / 2, h / 2, radius, start, start + 2 * Math.PI * frac);
                cr.stroke();
            }
        } else {
            roundedRect(cr, 0, 0, w, h);
            cr.setSourceRGBA(r, g, b, 0.2);
            cr.fill();
            if (frac > 0) {
                roundedRect(cr, 0, 0, Math.max(h, w * frac), h);
                cr.setSourceRGBA(r, g, b, 1);
                cr.fill();
            }
        }
        cr.$dispose();
    }
});

// Hourly bars, oldest → newest; the last (current, partial) hour is drawn fainter.
// Color comes from CSS; hovering reports the bar index (-1 on leave).
const HourlyChart = GObject.registerClass(
class AgentBarHourlyChart extends St.DrawingArea {
    _init(buckets, onHover) {
        super._init({style_class: 'agent-bar-chart', reactive: true, x_expand: true});
        this._buckets = buckets;
        this._onHover = onHover;
        this._hover = -1;
    }

    _slotAt(x) {
        const [, lx] = this.transform_stage_point(x, 0);
        const n = this._buckets.length;
        return Math.min(n - 1, Math.max(0, Math.floor(lx / (this.width / n))));
    }

    vfunc_motion_event(event) {
        const i = this._slotAt(event.get_coords()[0]);
        if (i !== this._hover) {
            this._hover = i;
            this._onHover(i);
            this.queue_repaint();
        }
        return Clutter.EVENT_PROPAGATE;
    }

    vfunc_leave_event(event) {
        this._hover = -1;
        this._onHover(-1);
        this.queue_repaint();
        return super.vfunc_leave_event(event);
    }

    vfunc_repaint() {
        const cr = this.get_context();
        const [w, h] = this.get_surface_size();
        const c = this.get_theme_node().get_foreground_color();
        const [r, g, b] = [c.red / 255, c.green / 255, c.blue / 255];
        const n = this._buckets.length;
        const gap = 2;
        const bw = (w - gap * (n - 1)) / n;
        const max = Math.max(...this._buckets.map(x => x.cost_usd));

        // Recessive baseline.
        cr.setSourceRGBA(r, g, b, 0.25);
        cr.rectangle(0, h - 1, w, 1);
        cr.fill();

        this._buckets.forEach((bucket, i) => {
            if (!(max > 0) || bucket.cost_usd <= 0)
                return;
            const bh = Math.max(2, (h - 1) * bucket.cost_usd / max);
            const x = i * (bw + gap);
            const current = i === n - 1;
            const dimmed = this._hover >= 0 && i !== this._hover;
            cr.setSourceRGBA(r, g, b, dimmed ? 0.35 : current ? 0.6 : 1);
            topRoundedBar(cr, x, h - 1 - bh, bw, bh, Math.min(4, bw / 2));
            cr.fill();
        });
        cr.$dispose();
    }
});

// Bar with rounded data-end (top), square at the baseline.
function topRoundedBar(cr, x, y, w, h, rad) {
    rad = Math.min(rad, h);
    cr.newSubPath();
    cr.moveTo(x, y + h);
    cr.lineTo(x, y + rad);
    cr.arc(x + rad, y + rad, rad, Math.PI, 3 * Math.PI / 2);
    cr.lineTo(x + w - rad, y);
    cr.arc(x + w - rad, y + rad, rad, 3 * Math.PI / 2, 2 * Math.PI);
    cr.lineTo(x + w, y + h);
    cr.closePath();
}

function setLevel(actor, lvl) {
    for (const l of LEVELS)
        actor.remove_style_class_name(l);
    actor.add_style_class_name(lvl);
}

function roundedRect(cr, x, y, w, h) {
    const rad = h / 2;
    cr.newSubPath();
    cr.arc(x + w - rad, y + rad, rad, -Math.PI / 2, Math.PI / 2);
    cr.arc(x + rad, y + rad, rad, Math.PI / 2, 3 * Math.PI / 2);
    cr.closePath();
}

export const Indicator = GObject.registerClass(
class AgentBarIndicator extends PanelMenu.Button {
    // iconsDir: Gio.File holding optional `<provider id>-symbolic.svg` logos.
    _init(settings, iconsDir) {
        super._init(0.5, 'Agent Bar');
        this._settings = settings;
        this._iconsDir = iconsDir;
        this._icons = new Map();
        this._snapshot = null;
        this._daemonUp = false;
        this._onRefresh = null;
        this._onPrefs = null;
        this._renderId = 0;

        const box = new St.BoxLayout({style_class: 'panel-status-menu-box'});
        this._logo = new St.Icon({style_class: 'system-status-icon agent-bar-logo', visible: false});
        this._ring = new Meter('ring', 'agent-bar-ring');
        this._label = new St.Label({
            text: '–',
            style_class: 'agent-bar-label',
            y_align: Clutter.ActorAlign.CENTER,
        });
        box.add_child(this._logo);
        box.add_child(this._ring);
        box.add_child(this._label);
        this.add_child(box);

        this._content = new PopupMenu.PopupMenuSection();
        this.menu.addMenuItem(this._content);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._refreshItem = this.menu.addAction('Refresh', () => this._onRefresh?.());
        this.menu.addAction('Settings', () => this._onPrefs?.());
        this._footer = new PopupMenu.PopupMenuItem('', {reactive: false, style_class: 'agent-bar-footer'});
        this._footer.label.opacity = DIM_OPACITY;
        this.menu.addMenuItem(this._footer);

        this._settingsIds = ['panel-limit', 'panel-icon', 'stats-range', 'show-remaining', 'show-countdown']
            .map(key => this._settings.connect(`changed::${key}`, () => this._queueRender()));
        this.menu.connect('open-state-changed', (_menu, open) => {
            if (open)
                this._render();
        });
        // Countdowns and "updated … ago" age even when the data doesn't change.
        this._tickId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, TICK_SECONDS, () => {
            this._render();
            return GLib.SOURCE_CONTINUE;
        });

        this._render();
    }

    setRefreshHandler(fn) {
        this._onRefresh = fn;
    }

    setPrefsHandler(fn) {
        this._onPrefs = fn;
    }

    setDaemonUp(up) {
        this._daemonUp = up;
        this._render();
    }

    setSnapshot(snapshot) {
        this._snapshot = snapshot;
        this._render();
    }

    // Deferred so a range button can't be destroyed inside its own click handler.
    _queueRender() {
        if (this._renderId)
            return;
        this._renderId = GLib.idle_add(GLib.PRIORITY_DEFAULT_IDLE, () => {
            this._renderId = 0;
            this._render();
            return GLib.SOURCE_REMOVE;
        });
    }

    _render() {
        // The Shell can destroy us (e.g. on shutdown) before disable() drops late D-Bus callbacks.
        if (this._destroyed)
            return;
        const providers = this._daemonUp ? this._snapshot?.providers ?? [] : [];
        this._renderPanel(providers[0]);
        this._renderMenu(providers);
    }

    _renderPanel(p) {
        const limit = fmt.pickLimit(p?.limits, this._settings.get_string('panel-limit'));
        if (!limit) {
            this._label.text = this._daemonUp ? '…' : '–';
            this._setGlyph(p?.id ?? 'claude', 0, 'normal', true);
            return;
        }
        const {pct} = fmt.shown(limit, this._settings.get_boolean('show-remaining'));
        let text = fmt.percent(pct);
        const until = fmt.untilReset(limit.resets_at);
        if (until && this._settings.get_boolean('show-countdown'))
            text += ` · ${until}`;
        this._label.text = text;
        this._setGlyph(p.id, pct, fmt.level(limit), p.status !== 'ok');
    }

    // Provider logo (tinted by level) if available and chosen, else the ring gauge.
    _setGlyph(providerId, pct, lvl, dim) {
        const gicon = this._settings.get_string('panel-icon') === 'logo' ? this._providerIcon(providerId) : null;
        this._logo.visible = !!gicon;
        this._ring.visible = !gicon;
        if (gicon) {
            this._logo.gicon = gicon;
            setLevel(this._logo, lvl);
        } else {
            this._ring.setValue(pct, lvl);
        }
        (gicon ? this._logo : this._ring).opacity = dim ? 128 : 255;
    }

    _providerIcon(id) {
        if (!this._icons.has(id)) {
            const file = this._iconsDir.get_child(`${id}-symbolic.svg`);
            this._icons.set(id, file.query_exists(null) ? new Gio.FileIcon({file}) : null);
        }
        return this._icons.get(id);
    }

    _renderMenu(providers) {
        this._content.removeAll();

        if (!this._daemonUp) {
            this._heading(this._content, 'Daemon not running');
            this._row(this._content, 'Start it with: systemctl --user start agent-bar', null, 'dim');
            this._refreshItem.visible = false;
            this._footer.visible = false;
            return;
        }
        this._refreshItem.visible = true;
        this._footer.visible = true;
        if (!providers.length) {
            this._row(this._content, 'Waiting for data…', null, 'dim');
            this._footer.label.text = '';
            return;
        }

        for (const p of providers)
            this._renderProvider(p);

        this._footer.label.text = `Limits updated ${fmt.ago(providers[0].limits_updated_at)}`;
    }

    _renderProvider(p) {
        const section = this._content;
        const plan = p.plan ? ` · ${p.plan[0].toUpperCase()}${p.plan.slice(1)}` : '';
        this._heading(section, `${p.name}${plan}`, p.status !== 'ok' ? p.status : null);
        if (p.status !== 'ok' && p.error)
            this._wrappedRow(section, p.error, 'agent-bar-error');

        for (const limit of p.limits)
            this._limitItem(section, limit);

        if (p.hourly?.length)
            this._hourlyChart(section, p.hourly);

        // Usage stats for the selected range.
        section.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        const range = this._settings.get_string('stats-range');
        this._rangeSwitcher(section, range);
        const s = p.stats?.[range];
        if (s) {
            this._row(section, 'Est. cost', fmt.cost(s.cost_usd), 'strong');
            this._row(section, 'Tokens', fmt.tokens(s.total_tokens), 'strong');
            this._row(section, 'Input / output', `${fmt.tokens(s.input)} / ${fmt.tokens(s.output)}`, 'sub');
            this._row(section, 'Cache read / write', `${fmt.tokens(s.cache_read)} / ${fmt.tokens(s.cache_write)}`, 'sub');
            this._row(section, 'Requests · sessions', `${s.requests} · ${s.sessions}`, 'sub');
        }
        if (s?.by_model.length) {
            section.addMenuItem(new PopupMenu.PopupSeparatorMenuItem('By model'));
            for (const m of s.by_model.slice(0, 3))
                this._row(section, fmt.shortModel(m.model), `${fmt.cost(m.cost_usd)} · ${fmt.tokens(m.total_tokens)}`, 'sub');
        }

        const shares = (p.breakdown ?? []).filter(r => r.pct > 0).sort((a, b) => b.pct - a.pct);
        if (shares.length) {
            section.addMenuItem(new PopupMenu.PopupSeparatorMenuItem('This week by product'));
            for (const r of shares)
                this._row(section, r.label, fmt.percent(r.pct), 'sub');
        }
    }

    _limitItem(section, limit) {
        const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false, style_class: 'agent-bar-limit'});
        const col = new St.BoxLayout({orientation: Clutter.Orientation.VERTICAL, x_expand: true});

        // Bars drain as quota is used; color still tracks usage level.
        const {pct, text} = fmt.shown(limit, this._settings.get_boolean('show-remaining'));
        const header = new St.BoxLayout({x_expand: true});
        header.add_child(new St.Label({text: limit.label, x_expand: true}));
        header.add_child(new St.Label({text, style_class: 'agent-bar-value'}));
        col.add_child(header);

        const bar = new Meter('bar', 'agent-bar-bar');
        bar.x_expand = true;
        bar.setValue(pct, fmt.level(limit));
        col.add_child(bar);

        const until = fmt.untilReset(limit.resets_at);
        if (until)
            col.add_child(new St.Label({text: `Resets in ${until}`, style_class: 'agent-bar-dim', opacity: DIM_OPACITY}));

        item.add_child(col);
        section.addMenuItem(item);
    }

    _hourlyChart(section, buckets) {
        section.addMenuItem(new PopupMenu.PopupSeparatorMenuItem('Last 24 hours · est. cost'));
        const summary = fmt.hourlySummary(buckets);
        const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        const col = new St.BoxLayout({orientation: Clutter.Orientation.VERTICAL, x_expand: true});
        const caption = new St.BoxLayout({x_expand: true});
        const left = new St.Label({x_expand: true, style_class: 'agent-bar-dim', opacity: DIM_OPACITY});
        const right = new St.Label({style_class: 'agent-bar-value'});
        caption.add_child(left);
        caption.add_child(right);

        const showDefault = () => {
            left.text = summary?.peak ?? 'No usage in the last 24 hours';
            right.text = summary?.total ?? '';
        };
        const chart = new HourlyChart(buckets, i => {
            if (i < 0)
                return showDefault();
            const b = buckets[i];
            const now = i === buckets.length - 1 ? ' (now)' : '';
            left.text = `${fmt.hourLabel(b.start)}${now} · ${fmt.tokens(b.total_tokens)} tokens`;
            right.text = fmt.cost(b.cost_usd);
        });
        showDefault();

        col.add_child(chart);
        col.add_child(caption);
        item.add_child(col);
        section.addMenuItem(item);
    }

    _rangeSwitcher(section, current) {
        const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        const box = new St.BoxLayout({style_class: 'agent-bar-ranges', x_expand: true});
        for (const [id, label] of fmt.RANGES) {
            const btn = new St.Button({label, style_class: 'agent-bar-range button', can_focus: true, x_expand: true});
            if (id === current)
                btn.add_style_pseudo_class('checked');
            btn.connect('clicked', () => this._settings.set_string('stats-range', id));
            box.add_child(btn);
        }
        item.add_child(box);
        section.addMenuItem(item);
    }

    // Long text (errors) wraps at the CSS max-width instead of widening the menu.
    _wrappedRow(section, text, styleClass) {
        const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        const label = new St.Label({text, style_class: styleClass, x_expand: true});
        label.clutter_text.line_wrap = true;
        label.clutter_text.line_wrap_mode = Pango.WrapMode.WORD_CHAR;
        label.clutter_text.ellipsize = Pango.EllipsizeMode.NONE;
        item.add_child(label);
        section.addMenuItem(item);
    }

    _heading(section, text, badge = null) {
        const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        item.add_child(new St.Label({text, style_class: 'agent-bar-heading', x_expand: true}));
        if (badge)
            item.add_child(new St.Label({text: badge, style_class: 'agent-bar-badge'}));
        section.addMenuItem(item);
    }

    _row(section, left, right, kind = '') {
        const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false, style_class: `agent-bar-row ${kind}`});
        const dim = kind === 'sub' || kind === 'dim';
        item.add_child(new St.Label({text: left, x_expand: true, style_class: 'agent-bar-key', opacity: dim ? DIM_OPACITY : 255}));
        if (right !== null)
            item.add_child(new St.Label({text: right, style_class: 'agent-bar-value'}));
        section.addMenuItem(item);
    }

    _onDestroy() {
        this._destroyed = true;
        if (this._tickId)
            GLib.source_remove(this._tickId);
        if (this._renderId)
            GLib.source_remove(this._renderId);
        this._tickId = this._renderId = 0;
        this._settingsIds.forEach(id => this._settings.disconnect(id));
        this._settingsIds = [];
        this._settings = null;
        this._onRefresh = null;
        this._onPrefs = null;
        super._onDestroy();
    }
});
