#!/usr/bin/env python3
"""Isolated portal/tray protocol regression; requires python-dbus + PyGObject.
Runs the virtual KWin focus test with mock portal activation instead of CLI show.
Never connects to the user's session bus or injects physical keyboard input.
"""
import os
from pathlib import Path
import subprocess
import sys

if '--child' not in sys.argv:
    raise SystemExit(subprocess.call(['dbus-run-session', '--', sys.executable, __file__, '--child']))

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib
DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
portal_name = dbus.service.BusName('org.freedesktop.portal.Desktop', bus)
watcher_name = None
objects = []
verified_trays = []
verification_errors = []


class Request(dbus.service.Object):
    @dbus.service.signal('org.freedesktop.portal.Request', signature='ua{sv}')
    def Response(self, code, values):
        pass

    @dbus.service.method('org.freedesktop.portal.Request', in_signature='', out_signature='')
    def Close(self):
        pass


class Session(dbus.service.Object):
    @dbus.service.signal('org.freedesktop.portal.Session', signature='a{sv}')
    def Closed(self, values):
        pass

    @dbus.service.method('org.freedesktop.portal.Session', in_signature='', out_signature='')
    def Close(self):
        self.Closed({})


class Portal(dbus.service.Object):
    def __init__(self):
        super().__init__(bus, '/org/freedesktop/portal/desktop')
        self.identities = {}
        self.session = None

    @dbus.service.method('org.freedesktop.DBus.Properties', in_signature='ss', out_signature='v')
    def Get(self, interface, name):
        return dbus.UInt32(2)

    @dbus.service.method('org.freedesktop.DBus.Properties', in_signature='s', out_signature='a{sv}')
    def GetAll(self, interface):
        return {'version': dbus.UInt32(2)}

    @dbus.service.method('org.freedesktop.host.portal.Registry', in_signature='sa{sv}', out_signature='', sender_keyword='sender')
    def Register(self, app_id, options, sender=None):
        assert app_id == 'org.tay.Tay', app_id
        self.identities[sender] = str(app_id)

    def request(self, sender, options, values):
        path = '/org/freedesktop/portal/desktop/request/' + sender[1:].replace('.', '_') + '/' + str(options['handle_token'])
        request = Request(bus, path)
        objects.append(request)
        GLib.timeout_add(25, lambda: (request.Response(0, values), False)[1])
        return dbus.ObjectPath(path)

    @dbus.service.method('org.freedesktop.portal.GlobalShortcuts', in_signature='a{sv}', out_signature='o', sender_keyword='sender')
    def CreateSession(self, options, sender=None):
        assert self.identities.get(sender) == 'org.tay.Tay', 'identity must precede session creation'
        path = '/org/freedesktop/portal/desktop/session/' + sender[1:].replace('.', '_') + '/' + str(options['session_handle_token'])
        self.active_session = Session(bus, path)
        objects.append(self.active_session)
        self.session = path
        return self.request(sender, options, {'session_handle': dbus.String(path)})

    @dbus.service.method('org.freedesktop.portal.GlobalShortcuts', in_signature='oa(sa{sv})sa{sv}', out_signature='o', sender_keyword='sender')
    def BindShortcuts(self, session, shortcuts, parent, options, sender=None):
        assert shortcuts[0][0] == 'toggle', shortcuts
        result = self.request(sender, options, {'shortcuts': dbus.Array([
            dbus.Struct(('toggle', dbus.Dictionary({'trigger_description': dbus.String('Ctrl+`'), 'description': dbus.String('呼出 / 隐藏 Tay 终端')}, signature='sv')))
        ], signature='(sa{sv})')})
        # Buffer complete press/release pairs before BindShortcuts responds.
        # Splitting signal types into independent queues loses these presses.
        for timestamp in range(20):
            self.Activated(session, 'toggle', timestamp * 2, {})
            self.Deactivated(session, 'toggle', timestamp * 2 + 1, {})
        return result

    @dbus.service.signal('org.freedesktop.portal.GlobalShortcuts', signature='osta{sv}')
    def Activated(self, session, shortcut, timestamp, options):
        pass

    @dbus.service.signal('org.freedesktop.portal.GlobalShortcuts', signature='osta{sv}')
    def Deactivated(self, session, shortcut, timestamp, options):
        pass

    @dbus.service.method('org.tay.Test', in_signature='', out_signature='b')
    def Ready(self):
        return self.session is not None

    @dbus.service.method('org.tay.Test', in_signature='', out_signature='')
    def ResetSession(self):
        self.active_session.Closed({})
        self.session = None

    @dbus.service.method('org.tay.Test', in_signature='', out_signature='')
    def Pulse(self):
        assert self.session is not None
        # An unrelated session must be ignored; two own-session activations
        # before release must result in precisely one toggle.
        self.Activated('/unrelated_session', 'toggle', 1, {})
        self.Activated(self.session, 'toggle', 2, {})
        self.Activated(self.session, 'toggle', 2, {})
        self.Deactivated(self.session, 'toggle', 3, {})


class Watcher(dbus.service.Object):
    def __init__(self):
        super().__init__(bus, '/StatusNotifierWatcher')
        self.items = []

    @dbus.service.method('org.freedesktop.DBus.Properties', in_signature='ss', out_signature='v')
    def Get(self, interface, name):
        if name == 'IsStatusNotifierHostRegistered': return dbus.Boolean(True)
        if name == 'ProtocolVersion': return dbus.Int32(0)
        return dbus.Array(self.items, signature='s')

    @dbus.service.method('org.freedesktop.DBus.Properties', in_signature='s', out_signature='a{sv}')
    def GetAll(self, interface):
        return {'IsStatusNotifierHostRegistered': dbus.Boolean(True), 'ProtocolVersion': dbus.Int32(0),
                'RegisteredStatusNotifierItems': dbus.Array(self.items, signature='s')}

    @dbus.service.method('org.kde.StatusNotifierWatcher', in_signature='s', out_signature='', sender_keyword='sender')
    def RegisterStatusNotifierItem(self, service, sender=None):
        name, path = (sender, service) if service.startswith('/') else (service, '/StatusNotifierItem')
        self.items.append(str(name) + str(path))
        # Query actual exported properties and menu, rather than just trusting registration.
        def verify():
            try:
                obj = bus.get_object(name, path)
                props = dbus.Interface(obj, 'org.freedesktop.DBus.Properties')
                assert str(props.Get('org.kde.StatusNotifierItem', 'Id')) == 'org.tay.Tay'
                assert str(props.Get('org.kde.StatusNotifierItem', 'IconName')) == 'tay-owl-rounded-v1'
                menu_path = props.Get('org.kde.StatusNotifierItem', 'Menu')
                menu = dbus.Interface(bus.get_object(name, menu_path), 'com.canonical.dbusmenu')
                layout = menu.GetLayout(0, -1, dbus.Array([], signature='s'))
                assert '显示 Tay' in str(layout) and '退出 Tay' in str(layout), layout
                verified_trays.append(str(name))
                print('PASS: actual tray properties, rounded icon and menu exported', flush=True)
                return False
            except Exception as error:
                verification_errors.append(repr(error))
                return False
        GLib.timeout_add(150, verify)


portal = Portal()
watcher = None

def late_watcher():
    global watcher_name, watcher
    watcher_name = dbus.service.BusName('org.kde.StatusNotifierWatcher', bus)
    watcher = Watcher()
    print('TEST: tray watcher appeared 5s after Tay startup', flush=True)
    return False

GLib.timeout_add(5000, late_watcher)
env = dict(os.environ, TAY_TEST_DESKTOP_BUS=os.environ['DBUS_SESSION_BUS_ADDRESS'], TAY_TEST_USE_PORTAL='1')
process = subprocess.Popen([sys.executable, str(Path(__file__).with_name('focus-smoke.py'))], env=env)
loop = GLib.MainLoop()


def finished():
    if process.poll() is None:
        return True
    loop.quit()
    return False


GLib.timeout_add(100, finished)
loop.run()
assert len(verified_trays) >= 2, ('tray properties/menu not verified', verification_errors)
assert not verification_errors, verification_errors
raise SystemExit(process.returncode)
