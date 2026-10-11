import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as MessageTray from 'resource:///org/gnome/shell/ui/messageTray.js';

import {dueAlerts} from './alerts.js';
import * as fmt from './format.js';

export class Notifier {
    constructor(settings) {
        this._settings = settings;
        this._sent = new Set();
        this._source = null;
    }

    check(snapshot) {
        if (!this._settings.get_boolean('notify-enabled'))
            return;
        const thresholds = {
            warnLeft: this._settings.get_int('notify-warn-left'),
            criticalLeft: this._settings.get_int('notify-critical-left'),
        };
        for (const alert of dueAlerts(snapshot.providers, thresholds, this._sent))
            this._notify(alert);
    }

    _notify({provider, limit, level, left}) {
        const title = level === 'critical'
            ? `${provider.name} ${limit.label} limit almost used up`
            : `${provider.name} ${limit.label} limit running low`;
        const until = fmt.untilReset(limit.resets_at);
        const body = `${fmt.percent(left)} left${until ? ` · resets in ${until}` : ''}`;
        const source = this._getSource();
        source.addNotification(new MessageTray.Notification({
            source,
            title,
            body,
            urgency: level === 'critical' ? MessageTray.Urgency.HIGH : MessageTray.Urgency.NORMAL,
        }));
    }

    // The tray destroys a source once its last notification is gone.
    _getSource() {
        if (!this._source) {
            this._source = new MessageTray.Source({title: 'Agent Bar', iconName: 'dialog-warning-symbolic'});
            this._source.connect('destroy', () => {
                this._source = null;
            });
            Main.messageTray.add(this._source);
        }
        return this._source;
    }

    destroy() {
        this._source?.destroy();
        this._source = null;
        this._settings = null;
    }
}
