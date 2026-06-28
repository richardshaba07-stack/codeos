# Indovina la Canzone 🎵

Party game musicale, trasformato in **app installabile (PWA)** per iPhone.
Niente App Store, niente costi: si pubblica gratis e si aggiunge alla schermata Home.

## Cosa c'è in questa cartella

| File | A cosa serve |
|------|--------------|
| `index.html` | Il gioco (ottimizzato per telefono) |
| `manifest.webmanifest` | Dice al telefono nome, icona e colori dell'app |
| `sw.js` | Service worker: la fa funzionare anche **offline** e la rende installabile |
| `icons/` | Le icone dell'app (PNG, varie misure per iPhone/iPad) |

> ⚠️ I video di YouTube ovviamente hanno **sempre** bisogno di internet.
> Offline si apre solo la schermata del gioco, non i brani.

---

## 1. Pubblicarla gratis con GitHub Pages

1. Su GitHub apri il repository → **Settings** → **Pages**.
2. In *Build and deployment* → *Source*: scegli **Deploy from a branch**.
3. Seleziona il branch che contiene questi file e cartella **/ (root)**, poi **Save**.
4. Dopo 1–2 minuti l'indirizzo dell'app sarà:

   ```
   https://<tuo-utente>.github.io/<nome-repo>/indovina-la-canzone/
   ```

   (Se i file fossero alla radice del repo, togli `/indovina-la-canzone/`.)

> 💡 Alternativa ancora più semplice senza Git: vai su **netlify.com/drop**,
> trascina la cartella `indovina-la-canzone` e ottieni subito un link `https://…`.
> Funziona uguale (serve un account gratuito).

---

## 2. Installarla su iPhone (sembra un'app vera)

1. Apri il link **in Safari** (non Chrome: solo Safari permette di installarla su iOS).
2. Tocca il pulsante **Condividi** (il quadrato con la freccia in su ⬆️).
3. Scegli **Aggiungi a Home** / *Aggiungi alla schermata Home*.
4. Conferma: comparirà l'icona con le barre arancioni 🎚️.

Aprendola da lì parte **a schermo intero**, senza la barra di Safari: identica a un'app.

---

## Aggiornare l'app dopo una modifica

Se cambi `index.html` (o altri file), il service worker tiene una copia in cache.
Per forzare l'aggiornamento sui telefoni già "installati", apri `sw.js` e cambia
il numero di versione:

```js
const CACHE = 'ilc-v2';   // -> 'ilc-v3', 'ilc-v4', ...
```

Al successivo avvio con internet, l'app scaricherà la versione nuova.

---

## Note tecniche

- **iPhone/iOS**: meta tag `apple-mobile-web-app-capable` + `apple-touch-icon` PNG,
  barra di stato translucida e rispetto del *notch* (`safe-area-inset`).
- **Altezza schermo**: usa `100dvh`, quindi niente "salti" con la barra del browser.
- **Service worker**: cache *stale-while-revalidate* solo per i file dell'app;
  YouTube e Google Fonts passano sempre dalla rete.
- Il service worker si attiva solo su **https** (cioè una volta pubblicata online),
  non aprendo il file direttamente dal computer.
