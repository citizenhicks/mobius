import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

// Exercise the bundled host controls, including a rapid hide/show during disconnect.
class RFB {
    static instances = [];
    constructor() { this.events = new Map(); RFB.instances.push(this); }
    addEventListener(name, callback) { this.events.set(name, callback); }
    disconnect() { this.disconnecting = true; }
    emit(name) { this.events.get(name)?.(); }
}
const window = { addEventListener() {} };
const document = {
    getElementById() { return {}; },
    documentElement: { classList: { toggle() {} }, style: { setProperty() {} } },
};
const source = readFileSync(new URL('../assets/desktop-viewer/viewer.js', import.meta.url), 'utf8')
    .replace(/^import .*;\n/gm, '');
vm.runInNewContext(source, { RFB, window, document, location: { protocol: 'file:' } });
window.desktop.connect('ws://viewer/rfb');
window.desktop.setControl(true);
window.desktop.setFit(false);
const first = RFB.instances[0];
first.emit('connect');
window.desktop.setVisible(false);
assert.equal(first.disconnecting, true);
assert.equal(first.viewOnly, true);
window.desktop.setVisible(true);
assert.equal(RFB.instances.length, 1, 'resume waits for the old socket to close');
first.emit('disconnect');
assert.equal(RFB.instances.length, 2, 'resume creates a fresh RFB handshake');
const resumed = RFB.instances[1];
assert.equal(resumed.viewOnly, true, 'control must be explicitly acquired again');
assert.equal(resumed.scaleViewport, false, 'fit preference survives pause');
window.desktop.setVisible(false);
window.desktop.setVisible(true);
window.desktop.setVisible(false);
resumed.emit('disconnect');
assert.equal(RFB.instances.length, 2, 'a hidden viewer must stay paused');
window.desktop.setVisible(true);
assert.equal(RFB.instances.length, 3);
first.emit('disconnect');
assert.equal(RFB.instances.length, 3, 'old events cannot restart a newer viewer');
console.log('Desktop viewer pause, resume, full handshake, control and fit checks passed.');
