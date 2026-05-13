// LG webOS / Magic Remote key bindings.
// Use: initRemote(); setRemoteHandlers({ ok: () => {}, channelUp: () => {}, ... });
//
// Arrow / channel keys auto-repeat with acceleration on hold (slow → fast, capped).

var NAMES = {
  37: 'arrowLeft', 38: 'arrowUp', 39: 'arrowRight', 40: 'arrowDown',
  13: 'ok',
  461: 'back', 8: 'back', 27: 'back',
  403: 'red', 404: 'green', 405: 'yellow', 406: 'blue',
  // Laptop dev aliases for the colour buttons — F1=red, F2=green, F3=yellow, F4=blue.
  112: 'red', 113: 'green', 114: 'yellow', 115: 'blue',
  427: 'channelUp', 428: 'channelDown',
  // PageUp/PageDown as channelUp/Down on laptop.
  33: 'channelUp', 34: 'channelDown',
  // Home / End → jump to top / bottom of list.
  36: 'home', 35: 'end'
};

// Letter aliases for the colour buttons (laptop only). Suppressed while typing into
// an input so they don't hijack search.
var LETTER_NAMES = { 82: 'red', 71: 'green', 89: 'yellow', 66: 'blue' };

var REPEATABLE = {
  arrowLeft: true, arrowRight: true, arrowUp: true, arrowDown: true,
  channelUp: true, channelDown: true
};

var INITIAL_DELAY_MS = 300;
var MIN_INTERVAL_MS = 25;
var ACCEL = 0.75;

var handlers = {};
var started = false;
var holdKey = null;
var holdTimer = null;
var holdInterval = INITIAL_DELAY_MS;

function call(name) {
  if (handlers[name]) handlers[name]();
  if (handlers.any) handlers.any(name);
}

function scheduleNext() {
  holdTimer = setTimeout(function () {
    if (!holdKey) return;
    call(holdKey);
    holdInterval = Math.max(MIN_INTERVAL_MS, holdInterval * ACCEL);
    scheduleNext();
  }, holdInterval);
}

function startHold(name) {
  stopHold();
  holdKey = name;
  holdInterval = INITIAL_DELAY_MS;
  scheduleNext();
}

function stopHold() {
  if (holdTimer) { clearTimeout(holdTimer); holdTimer = null; }
  holdKey = null;
  holdInterval = INITIAL_DELAY_MS;
}

function isInputFocused() {
  var ae = document.activeElement;
  return !!(ae && (ae.tagName === 'INPUT' || ae.tagName === 'TEXTAREA'));
}

// Make the input non-focusable so subsequent OK keypresses don't reopen the OS keyboard.
function exileInput(inp) {
  inp.blur();
  inp.setAttribute('readonly', 'readonly');
  inp.setAttribute('tabindex', '-1');
  inp.classList.add('exiled');
}

function onKeyDown(e) {
  if (e.repeat) {
    e.preventDefault();
    return;
  }
  var letterName = LETTER_NAMES[e.keyCode];
  if (letterName) {
    if (isInputFocused()) return;
    e.preventDefault();
    call(letterName);
    return;
  }
  var name = NAMES[e.keyCode];
  if (name) {
    if (isInputFocused()) exileInput(document.activeElement);
    e.preventDefault();
    call(name);
    if (REPEATABLE[name]) startHold(name);
  }
}

function onKeyUp(e) {
  var name = NAMES[e.keyCode];
  if (name && name === holdKey) stopHold();
}

export function setRemoteHandlers(map) {
  for (var k in map) handlers[k] = map[k];
}

export function initRemote() {
  if (started) return;
  started = true;
  window.addEventListener('keydown', onKeyDown);
  window.addEventListener('keyup', onKeyUp);
}
