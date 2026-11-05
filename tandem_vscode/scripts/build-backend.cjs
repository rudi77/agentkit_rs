const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const root = path.resolve(__dirname, '../..');
execFileSync('cargo', ['build', '--manifest-path', path.join(root, 'tandem_app/Cargo.toml'), '--release', '--features', 'openai'], { stdio: 'inherit' });
const binary = process.platform === 'win32' ? 'tandem.exe' : 'tandem';
fs.mkdirSync(path.join(__dirname, '../bin'), { recursive: true });
fs.copyFileSync(path.join(root, 'tandem_app/target/release', binary), path.join(__dirname, '../bin', binary));
