import Adw from 'gi://Adw';
import Gio from 'gi://Gio';
import Gtk from 'gi://Gtk';

import {ExtensionPreferences} from 'resource:///org/gnome/Shell/Extensions/js/extensions/prefs.js';

// [settings value, label] for the top bar limit; ids match the daemon's limit ids.
const PANEL_LIMITS = [
    ['auto', 'Closest to running out'],
    ['session', '5-hour'],
    ['weekly_all', 'Weekly'],
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
        display.add(this._switch(settings, 'show-countdown', 'Show reset countdown in the top bar'));
        display.add(this._panelLimitRow(settings));
        display.add(this._choiceRow(settings, 'panel-icon', 'Top bar icon', [
            ['logo', 'Provider logo'],
            ['ring', 'Progress ring'],
        ]));

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
