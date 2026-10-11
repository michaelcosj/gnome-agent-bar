import Gio from 'gi://Gio';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

import {Indicator} from './indicator.js';
import {Notifier} from './notifier.js';

// Must match daemon/src/dbus.rs.
const BUS_NAME = 'dev.agentbar.Usage';
const OBJECT_PATH = '/dev/agentbar/Usage';
const UsageProxy = Gio.DBusProxy.makeProxyWrapper(`
<node>
  <interface name="dev.agentbar.Usage">
    <method name="GetSnapshot"><arg type="s" direction="out" name="snapshot"/></method>
    <method name="Refresh"/>
    <signal name="Changed"><arg type="s" name="snapshot"/></signal>
  </interface>
</node>`);

export default class AgentBarExtension extends Extension {
    enable() {
        const settings = this.getSettings();
        this._indicator = new Indicator(settings, this.dir.get_child('icons'));
        this._indicator.setPrefsHandler(() => this.openPreferences());
        Main.panel.addToStatusArea(this.uuid, this._indicator);
        this._notifier = new Notifier(settings);

        this._cancellable = new Gio.Cancellable();
        // Async: never block the Shell on the bus.
        new UsageProxy(Gio.DBus.session, BUS_NAME, OBJECT_PATH, (proxy, error) => {
            if (error) {
                if (!error.matches(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                    console.warn(`agent-bar: D-Bus proxy failed: ${error.message}`);
                return;
            }
            if (!this._indicator)
                return; // disabled meanwhile
            this._proxy = proxy;
            this._changedId = proxy.connectSignal('Changed', (_p, _sender, [json]) => this._onSnapshot(json));
            this._ownerId = proxy.connect('notify::g-name-owner', () => this._syncOwner());
            this._indicator.setRefreshHandler(() => {
                proxy.RefreshAsync().catch(e => console.warn(`agent-bar: Refresh failed: ${e.message}`));
            });
            this._syncOwner();
        }, this._cancellable, Gio.DBusProxyFlags.DO_NOT_AUTO_START | Gio.DBusProxyFlags.DO_NOT_LOAD_PROPERTIES);
    }

    disable() {
        this._cancellable?.cancel();
        this._cancellable = null;
        if (this._proxy) {
            this._proxy.disconnectSignal(this._changedId);
            this._proxy.disconnect(this._ownerId);
            this._proxy = null;
        }
        this._indicator?.destroy();
        this._indicator = null;
        this._notifier?.destroy();
        this._notifier = null;
    }

    _onSnapshot(json) {
        if (!this._indicator)
            return;
        let snapshot;
        try {
            snapshot = JSON.parse(json);
        } catch (e) {
            console.warn(`agent-bar: bad snapshot: ${e.message}`);
            return;
        }
        this._indicator.setSnapshot(snapshot);
        this._notifier.check(snapshot);
    }

    // The daemon may start, stop or restart at any time.
    _syncOwner() {
        const up = !!this._proxy?.g_name_owner;
        this._indicator?.setDaemonUp(up);
        if (!up)
            return;
        this._proxy.GetSnapshotAsync()
            .then(([json]) => this._onSnapshot(json))
            .catch(e => console.warn(`agent-bar: GetSnapshot failed: ${e.message}`));
    }
}
