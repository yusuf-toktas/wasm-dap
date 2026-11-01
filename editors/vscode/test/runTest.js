'use strict';

// Runs the extension integration test inside VS Code.
//
// Environment:
//   WASM_DAP_BINARY    path to the wasm-dap executable under test (required)
//   VSCODE_EXECUTABLE  VS Code executable to use instead of downloading one

const path = require('path');
const fs = require('fs');
const os = require('os');
const { runTests } = require('@vscode/test-electron');

async function main() {
    const binary = process.env.WASM_DAP_BINARY;
    if (!binary || !fs.existsSync(binary)) {
        throw new Error('Set WASM_DAP_BINARY to the wasm-dap executable to test.');
    }

    const workspace = fs.mkdtempSync(path.join(os.tmpdir(), 'wasm-dap-vscode-'));
    fs.copyFileSync(path.join(__dirname, 'fixtures', 'counter.wat'), path.join(workspace, 'counter.wat'));
    fs.mkdirSync(path.join(workspace, '.vscode'));
    fs.writeFileSync(
        path.join(workspace, '.vscode', 'settings.json'),
        JSON.stringify({ 'wasm-dap.adapterPath': path.resolve(binary) }, null, 2)
    );

    const options = {
        extensionDevelopmentPath: path.resolve(__dirname, '..'),
        extensionTestsPath: path.resolve(__dirname, 'suite', 'index.js'),
        launchArgs: [workspace, '--disable-extensions', '--disable-workspace-trust'],
    };
    if (process.env.VSCODE_EXECUTABLE) {
        options.vscodeExecutablePath = process.env.VSCODE_EXECUTABLE;
    }
    try {
        await runTests(options);
    } finally {
        fs.rmSync(workspace, { recursive: true, force: true });
    }
}

main().catch((error) => {
    console.error(error);
    process.exit(1);
});
