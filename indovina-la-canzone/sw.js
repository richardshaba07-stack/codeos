/* Service worker per "Indovina la Canzone"
   Strategia: cache dell'app shell (stale-while-revalidate) per le risorse
   dello stesso dominio; tutto il resto (anteprime Deezer, Google Fonts) passa
   direttamente alla rete. Cambia CACHE per forzare un aggiornamento. */
const CACHE = 'ilc-v5';
const ASSETS = [
  './',
  './index.html',
  './manifest.webmanifest',
  './icons/icon-152.png',
  './icons/icon-167.png',
  './icons/icon-180.png',
  './icons/icon-192.png',
  './icons/icon-512.png'
];

self.addEventListener('install', event => {
  event.waitUntil(
    caches.open(CACHE)
      .then(cache => cache.addAll(ASSETS))
      .then(() => self.skipWaiting())
  );
});

self.addEventListener('activate', event => {
  event.waitUntil(
    caches.keys()
      .then(keys => Promise.all(keys.filter(k => k !== CACHE).map(k => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

self.addEventListener('fetch', event => {
  const req = event.request;
  if (req.method !== 'GET') return;

  const url = new URL(req.url);
  // Lascia passare alla rete tutto ciò che non è del nostro dominio
  // (i video YouTube e i font NON vanno messi in cache).
  if (url.origin !== self.location.origin) return;

  event.respondWith(
    caches.match(req).then(cached => {
      const network = fetch(req)
        .then(res => {
          if (res && res.status === 200 && res.type === 'basic') {
            const copy = res.clone();
            caches.open(CACHE).then(cache => cache.put(req, copy));
          }
          return res;
        })
        .catch(() => cached);
      return cached || network;
    })
  );
});
