// hum playback engine: Spotify Web Playback SDK inside castLabs Electron (Widevine).
// Spawned by the hum daemon. Protocol: JSON lines; stdin carries daemon -> player messages,
// stdout lines prefixed with "HUM " carry player -> daemon messages. Everything else is noise.
const { app, components, BrowserWindow } = require('electron');
const http = require('http');
const fs = require('fs');
const path = require('path');
const readline = require('readline');

const send = (msg) => process.stdout.write('HUM ' + JSON.stringify(msg) + '\n');

app.dock?.hide();
// No display server on headless Linux: render offscreen.
if (process.platform === 'linux' && !process.env.DISPLAY && !process.env.WAYLAND_DISPLAY) {
  app.commandLine.appendSwitch('ozone-platform', 'headless');
}
app.commandLine.appendSwitch('autoplay-policy', 'no-user-gesture-required');

// The SDK refuses file:// origins, so the page is served on loopback.
const page = fs.readFileSync(path.join(__dirname, 'player.html'));
const server = http.createServer((req, res) => {
  res.setHeader('content-type', 'text/html');
  res.end(page);
});

let win;

// Runs inside the SDK iframe. Chromium routes the media session (macOS Now Playing, MPRIS) to the
// frame that owns the media player, which is the SDK's cross-origin iframe, not our page: only the
// main process can reach it. Actions come back as "HUM_MEDIA" console messages.
function nowPlaying(st) {
  const ms = navigator.mediaSession;
  ms.metadata = new MediaMetadata({
    title: st.name,
    artist: st.artists,
    album: st.album,
    artwork: st.cover ? [{ src: st.cover, sizes: '300x300', type: 'image/jpeg' }] : [],
  });
  ms.playbackState = st.playing ? 'playing' : 'paused';
  if (st.duration_ms > 0) {
    ms.setPositionState({ duration: st.duration_ms / 1000, position: Math.min(st.position_ms, st.duration_ms) / 1000, playbackRate: 1 });
  }
  const act = (cmd) => (d) => console.log('HUM_MEDIA ' + JSON.stringify({ cmd, arg: Math.round((d.seekTime || 0) * 1000) }));
  const actions = { play: 'play', pause: 'pause', nexttrack: 'next', previoustrack: 'prev', seekto: 'seek' };
  for (const [action, cmd] of Object.entries(actions)) ms.setActionHandler(action, act(cmd));
}

const mirror = (st) => win?.webContents.mainFrame.framesInSubtree
  .find((f) => f.url.startsWith('https://sdk.scdn.co/'))
  ?.executeJavaScript(`(${nowPlaying})(${JSON.stringify(st)})`)
  .catch(() => {});
const call = (fn, ...args) => win?.webContents.executeJavaScript(`${fn}(...${JSON.stringify(args)})`).catch(() => {});

readline.createInterface({ input: process.stdin }).on('line', (line) => {
  let msg;
  try { msg = JSON.parse(line); } catch { return; }
  if (msg.token !== undefined) call('setToken', msg.token);
  if (msg.cmd) call('command', msg.cmd, msg.arg);
});
// Daemon gone: nothing left to play for.
process.stdin.on('end', () => app.quit());

app.on('window-all-closed', () => {});
app.whenReady().then(async () => {
  await components.whenReady();
  server.listen(0, '127.0.0.1', () => {
    win = new BrowserWindow({ show: false });
    win.webContents.on('console-message', (e) => {
      if (e.message.startsWith('HUM ')) {
        process.stdout.write(e.message + '\n');
        const st = JSON.parse(e.message.slice(4)).state;
        if (st?.uri) mirror(st);
      }
      if (e.message.startsWith('HUM_MEDIA ')) {
        const { cmd, arg } = JSON.parse(e.message.slice(10));
        call('command', cmd, arg);
      }
    });
    win.webContents.on('render-process-gone', (_, d) => {
      send({ error: `renderer gone: ${d.reason}` });
      app.exit(1);
    });
    win.loadURL(`http://127.0.0.1:${server.address().port}/`);
  });
}).catch((e) => {
  send({ error: `widevine: ${e}` });
  app.exit(1);
});
