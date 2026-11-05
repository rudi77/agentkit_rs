// Kleiner realer Quelltextcheck, kein C#-Lauf und kein Verhaltenstest.
const fs = require('node:fs');
const path = require('node:path');
const source = fs.readFileSync(path.join(__dirname, 'Normalize.cs'), 'utf8');
if (!source.includes('return input.Trim();')) {
  console.error('Quelltextcheck fehlgeschlagen: Trim fehlt.');
  process.exit(1);
}
console.log('Quelltextcheck bestanden: Trim ist vorhanden. Kein C#-Verhaltenstest.');
