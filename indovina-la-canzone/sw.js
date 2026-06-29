/* Service worker per "Indovina la Canzone"
   - Documento HTML / navigazione: NETWORK-FIRST -> quando sei online vedi
     SEMPRE l'ultima versione (niente più aggiornamenti "in ritardo"); offline
     usa la copia salvata.
   - Altri file dello stesso dominio (icone, manifest): stale-while-revalidate.
   - Anteprime Deezer e Google Fonts: passano direttamente alla rete. */
const CACHE = 'ilc-v11';
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
  if (url.origin !== self.location.origin) return;  // Deezer/Fonts: niente cache

  const isDoc = req.mode === 'navigate'
    || req.destination === 'document'
    || url.pathname.endsWith('/')
    || url.pathname.endsWith('index.html');

  if (isDoc) {
    // NETWORK-FIRST: prima la rete (versione fresca), poi la cache come riserva
    event.respondWith(
      fetch(req)
        .then(res => {
          const copy = res.clone();
          caches.open(CACHE).then(c => c.put(req, copy));
          return res;
        })
        .catch(() => caches.match(req).then(r => r || caches.match('./index.html')))
    );
    return;
  }

  // ALTRO (icone, manifest): stale-while-revalidate
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
