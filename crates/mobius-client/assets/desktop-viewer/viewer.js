import RFB from './novnc/core/rfb.js';
import KeyTable from './novnc/core/input/keysym.js';
import Keyboard from './novnc/core/input/keyboard.js';
import keysyms from './novnc/core/input/keysymdef.js';

let rfb;
let controlling = false;
let connected = false;
let visible = true;
let paused = false;
let connection;
let fit = true;
let mobileControls;
let keyboardInput;
let lastKeyboardInput = '';
const post = body => window.webkit?.messageHandlers?.desktop?.postMessage(body);

function viewerState(state) {
    connected = state === 'connected';
    document.title = `Computer · ${state}`;
    window.desktop.setControl(controlling);
    post({type: 'viewer-state', state});
}

function setMobileControls(enabled) {
    if (!enabled || mobileControls) return;
    document.documentElement.classList.add('mobile');
    mobileControls = document.createElement('div');
    mobileControls.id = 'mobile-controls';
    const makeButton = (label, image, action) => {
        const button = document.createElement('button');
        button.type = 'button';
        button.setAttribute('aria-label', label);
        const icon = document.createElement('img');
        icon.src = image;
        icon.alt = '';
        button.append(icon);
        button.addEventListener('click', action);
        mobileControls.append(button);
        return button;
    };
    const clipboard = makeButton('Clipboard', 'mobile-clipboard.svg', () => post({type: 'clipboard'}));
    clipboard.dataset.input = 'true';
    makeButton('Fit or actual size', 'mobile-fit.svg', () => window.desktop.setFit(!fit));
    const keyboard = makeButton('Show keyboard', 'mobile-keyboard.svg', () => {
        if (!controlling || !connected) return;
        if (document.activeElement === keyboardInput) keyboardInput.blur();
        else { keyboardInput.focus(); keyboardInput.setSelectionRange(keyboardInput.value.length, keyboardInput.value.length); }
    });
    keyboard.dataset.input = 'true';
    document.body.append(mobileControls);
    keyboardInput = document.createElement('textarea');
    keyboardInput.id = 'mobile-keyboard-input';
    keyboardInput.setAttribute('aria-label', 'Remote keyboard input');
    keyboardInput.setAttribute('autocapitalize', 'off');
    keyboardInput.setAttribute('autocorrect', 'off');
    keyboardInput.setAttribute('autocomplete', 'off');
    keyboardInput.spellcheck = false;
    const resetInput = () => { keyboardInput.value = '_'.repeat(64); lastKeyboardInput = keyboardInput.value; };
    resetInput();
    document.body.append(keyboardInput);
    // Reuse noVNC's keyboard normalization and mobile input fallback.
    const inputKeyboard = new Keyboard(keyboardInput);
    inputKeyboard.onkeyevent = (keysym, code, down) => { if (controlling) rfb?.sendKey(keysym, code, down); };
    inputKeyboard.grab();
    keyboardInput.addEventListener('input', event => {
        if (!controlling || event.isComposing) return;
        const next = Array.from(keyboardInput.value);
        const previous = Array.from(lastKeyboardInput);
        let shared = 0;
        while (shared < Math.min(next.length, previous.length) && next[shared] === previous[shared]) shared++;
        for (let index = shared; index < previous.length; index++) rfb?.sendKey(KeyTable.XK_BackSpace, 'Backspace');
        for (const character of next.slice(shared)) rfb?.sendKey(keysyms.lookup(character.codePointAt(0)));
        lastKeyboardInput = keyboardInput.value;
        if (next.length > 128 || next.length === 0) resetInput();
    });
    const keyboardChanged = () => {
        const active = document.activeElement === keyboardInput;
        keyboard.firstElementChild.src = active ? 'mobile-keyboard-hide.svg' : 'mobile-keyboard.svg';
        keyboard.setAttribute('aria-label', active ? 'Hide keyboard' : 'Show keyboard');
        if (rfb) rfb.focusOnClick = !active;
        if (!active) resetInput();
    };
    keyboardInput.addEventListener('focus', keyboardChanged);
    keyboardInput.addEventListener('blur', keyboardChanged);
    window.desktop.setControl(controlling);
}

// Command belongs to the host app. noVNC maps it to a remote modifier on macOS.
for (const type of ['keydown', 'keyup']) {
    window.addEventListener(type, function(event) {
        if (event.code === 'MetaLeft' || event.code === 'MetaRight') {
            event.preventDefault();
            event.stopImmediatePropagation();
        }
    }, true);
}

window.desktop = {
    connect(url, protocols = []) {
        connection = [url, protocols];
        if (!visible) return;
        const previous = rfb;
        rfb = undefined;
        previous?.disconnect();
        viewerState('connecting');
        const current = new RFB(document.getElementById('screen'), url,
                      { shared: true, wsProtocols: protocols });
        rfb = current;
        current.addEventListener('connect', () => {
            if (rfb === current) viewerState('connected');
        });
        current.addEventListener('disconnect', () => {
            if (rfb !== current) return;
            rfb = undefined;
            if (paused) {
                paused = false;
                if (visible) window.desktop.connect(...connection);
            } else viewerState('disconnected');
        });
        current.addEventListener('securityfailure', () => {
            if (rfb === current) viewerState('failed');
        });
        rfb.background = '#000';
        rfb.viewOnly = !controlling;
        rfb.scaleViewport = fit;
        rfb.resizeSession = false;
        rfb.dragViewport = !fit;
        rfb.clipViewport = true;
    },
    setVisible(enabled) {
        if (visible === enabled) return;
        visible = enabled;
        if (!enabled) {
            window.desktop.setControl(false);
            viewerState('paused');
            if (rfb) {
                paused = true;
                rfb.disconnect();
            }
        } else if (!paused && connection) {
            // A new RFB handshake requests the complete framebuffer on resume.
            window.desktop.connect(...connection);
        }
    },
    setControl(enabled, accent) {
        controlling = enabled;
        document.documentElement.classList.toggle('controlling', enabled);
        if (accent) document.documentElement.style.setProperty('--control-accent', accent);
        if (rfb) rfb.viewOnly = !enabled;
        if (!enabled) keyboardInput?.blur();
        mobileControls?.querySelectorAll('[data-input]').forEach(button => { button.disabled = !enabled || !connected; });
    },
    setMobileControls,
    setFit(enabled) {
        fit = enabled;
        if (rfb) {
            rfb.scaleViewport = enabled;
            rfb.dragViewport = !enabled;
        }
    },
    paste(text) {
        if (!rfb || !controlling || !connected) return;
        rfb.clipboardPasteFrom(text);
        rfb.sendKey(KeyTable.XK_Control_L, 'ControlLeft', true);
        rfb.sendKey(KeyTable.XK_v, 'KeyV');
        rfb.sendKey(KeyTable.XK_Control_L, 'ControlLeft', false);
    },
};

post({type: 'ready'});

if (location.protocol === 'http:') {
    const url = new URL('rfb', location.href);
    url.protocol = 'ws:';
    window.desktop.connect(url.href);
}
