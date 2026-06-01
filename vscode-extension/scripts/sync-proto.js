// Sincronizza il contratto gRPC nella cartella dell'estensione.
//
// L'unica fonte di verità è `crates/codeos-rpc/proto/codeos.proto`: qui lo
// copiamo dentro l'estensione (cartella `proto/`, gitignorata) così che finisca
// nel pacchetto .vsix e sia caricabile a runtime da `@grpc/proto-loader`.

const fs = require('fs');
const path = require('path');

const src = path.resolve(
  __dirname,
  '..',
  '..',
  'crates',
  'codeos-rpc',
  'proto',
  'codeos.proto',
);
const destDir = path.resolve(__dirname, '..', 'proto');
const dest = path.join(destDir, 'codeos.proto');

if (!fs.existsSync(src)) {
  console.error(`[sync-proto] sorgente non trovata: ${src}`);
  process.exit(1);
}

fs.mkdirSync(destDir, { recursive: true });
fs.copyFileSync(src, dest);
console.log(`[sync-proto] ${src} -> ${dest}`);
