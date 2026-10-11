import Adw from 'gi://Adw';
import Gio from 'gi://Gio';
import Gtk from 'gi://Gtk';

import {ExtensionPreferences} from 'resource:///org/gnome/Shell/Extensions/js/extensions/prefs.js';

// [settings value, label] for the top bar limit; the daemon maps ChatGPT's windows onto these.
const PANEL_LIMITS = [
    ['auto', 'Closest to running out'],
    ['session', 'Short window (5-hour)'],
    ['weekly_all', 'Weekly'],
];

// Account ids match the daemon's provider ids.
const ACCOUNTS = [
    ['claude', 'Claude'],
    ['openai', 'ChatGPT'],
];

export default class AgentBarPreferences extends ExtensionPreferences {
    fillPreferencesWindow(window) {
        const settings = this.getSettings();
        window._settings = settings; // keep alive for the window's lifetime
        window.set_default_size(560, 640);

        const page = new Adw.PreferencesPage({title: 'General', icon_name: 'preferences-system-symbolic'});
        window.add(page);

        const display = new Adw.PreferencesGroup({title: 'Display'});
        page.add(display);
        display.add(this._switch(settings, 'show-remaining', 'Show what is left',
            'Off shows what is used instead'));
        display.add(this._choiceRow(settings, 'panel-accounts', 'Accounts in the top bar', [
            ['all', 'All'],
            ['closest', 'Closest to its limit'],
        ]));
        display.add(this._panelLimitRow(settings));
        display.add(this._choiceRow(settings, 'panel-countdown', 'Reset countdown in the top bar', [
            ['closest', 'Account closest to its limit'],
            ['all', 'Every account'],
            ['none', 'Off'],
        ]));
        display.add(this._choiceRow(settings, 'panel-icon', 'Top bar icon', [
            ['logo', 'Provider logo'],
            ['ring', 'Progress ring'],
        ]));

        const accounts = new Adw.PreferencesGroup({
            title: 'Accounts',
            description: 'Accounts appear once you are signed in or have usage from Claude Code, Codex, pi or opencode.',
        });
        page.add(accounts);
        for (const [id, name] of ACCOUNTS)
            accounts.add(this._accountRow(settings, id, name));

        const notify = new Adw.PreferencesGroup({
            title: 'Notifications',
            description: 'Once per limit window; a new window after a reset can notify again.',
        });
        page.add(notify);
        notify.add(this._switch(settings, 'notify-enabled', 'Notify when a limit is running low'));
        for (const [key, title] of [
            ['notify-warn-left', 'Warn at % left'],
            ['notify-critical-left', 'Urgent at % left'],
        ]) {
            const row = Adw.SpinRow.new_with_range(0, 100, 1);
            row.title = title;
            settings.bind(key, row, 'value', Gio.SettingsBindFlags.DEFAULT);
            settings.bind('notify-enabled', row, 'sensitive', Gio.SettingsBindFlags.GET);
            notify.add(row);
        }

        const about = new Adw.PreferencesGroup({
            title: 'Daemon',
            description: 'Data comes from the agent-bar-daemon user service. '
                + 'Check it with “systemctl --user status agent-bar”.',
        });
        page.add(about);
    }

    _switch(settings, key, title, subtitle = '') {
        const row = new Adw.SwitchRow({title, subtitle});
        settings.bind(key, row, 'active', Gio.SettingsBindFlags.DEFAULT);
        return row;
    }

    // Switch that removes/adds the account id in the hidden-accounts list.
    _accountRow(settings, id, title) {
        const row = new Adw.SwitchRow({title});
        const sync = () => {
            row.active = !settings.get_strv('hidden-accounts').includes(id);
        };
        sync();
        const handler = settings.connect('changed::hidden-accounts', sync);
        row.connect('notify::active', () => {
            const hidden = settings.get_strv('hidden-accounts').filter(x => x !== id);
            if (!row.active)
                hidden.push(id);
            if (hidden.join() !== settings.get_strv('hidden-accounts').join())
                settings.set_strv('hidden-accounts', hidden);
        });
        row.connect('destroy', () => settings.disconnect(handler));
        return row;
    }

    _panelLimitRow(settings) {
        return this._choiceRow(settings, 'panel-limit', 'Limit in the top bar', PANEL_LIMITS);
    }

    // ComboRow bound to a string key; choices are [value, label] pairs.
    _choiceRow(settings, key, title, choices) {
        const row = new Adw.ComboRow({
            title,
            model: Gtk.StringList.new(choices.map(([, label]) => label)),
        });
        const sync = () => {
            const i = choices.findIndex(([value]) => value === settings.get_string(key));
            row.selected = Math.max(0, i);
        };
        sync();
        const id = settings.connect(`changed::${key}`, sync);
        row.connect('notify::selected', () => settings.set_string(key, choices[row.selected][0]));
        row.connect('destroy', () => settings.disconnect(id));
        return row;
    }
}
