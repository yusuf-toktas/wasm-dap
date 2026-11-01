'use strict';

const vscode = require('vscode');
const path = require('path');
const fs = require('fs');
const { execFile } = require('child_process');

const DEBUG_TYPE = 'wasm-dap';
const BINARY = process.platform === 'win32' ? 'wasm-dap.exe' : 'wasm-dap';

function isFile(candidate) {
    try {
        return fs.statSync(candidate).isFile();
    } catch {
        return false;
    }
}

/** True when `file` lies in `folder` or one of its subdirectories. */
function isInside(file, folder) {
    const relative = path.relative(folder, path.resolve(file));
    return relative !== '..' && !relative.startsWith('..' + path.sep) && !path.isAbsolute(relative);
}

function findOnPath(name) {
    const entries = (process.env.PATH || '').split(path.delimiter).filter(Boolean);
    for (const dir of entries) {
        const candidate = path.join(dir, name);
        if (isFile(candidate)) {
            return candidate;
        }
    }
    return undefined;
}

/** Restores the execute permission that archive extraction may drop. */
function ensureExecutable(file) {
    if (process.platform === 'win32' || !isFile(file)) {
        return;
    }
    try {
        fs.accessSync(file, fs.constants.X_OK);
    } catch {
        try {
            fs.chmodSync(file, 0o755);
        } catch {
            // Reported when the adapter fails to start.
        }
    }
}

/**
 * Locates the adapter executable: explicit setting, bundled binary, PATH,
 * then the build output of an open wasm-dap checkout.
 */
function resolveAdapter(context) {
    const configured = vscode.workspace.getConfiguration('wasm-dap').get('adapterPath');
    if (configured) {
        const folders = (vscode.workspace.workspaceFolders ?? []).map((f) => f.uri.fsPath);
        const expanded = configured.replace(/\$\{workspaceFolder\}/g, folders[0] ?? '');
        // Even from user settings, an untrusted workspace must not supply the binary.
        if (
            !vscode.workspace.isTrusted &&
            (configured.includes('${workspaceFolder}') || folders.some((f) => isInside(expanded, f)))
        ) {
            throw new Error('wasm-dap.adapterPath points into this workspace, which is not trusted.');
        }
        if (isFile(expanded)) {
            return expanded;
        }
        throw new Error(`wasm-dap.adapterPath points to "${expanded}", which does not exist.`);
    }
    const bundled = path.join(context.extensionPath, 'bin', BINARY);
    ensureExecutable(bundled);
    const candidates = [bundled];
    const onPath = findOnPath(BINARY);
    if (onPath) {
        candidates.push(onPath);
    }
    // Binaries inside the workspace are only run when the user trusts it.
    if (vscode.workspace.isTrusted) {
        for (const folder of vscode.workspace.workspaceFolders ?? []) {
            candidates.push(path.join(folder.uri.fsPath, 'target', 'release', BINARY));
            candidates.push(path.join(folder.uri.fsPath, 'target', 'debug', BINARY));
        }
    }
    candidates.push(path.join(context.extensionPath, '..', '..', 'target', 'release', BINARY));
    candidates.push(path.join(context.extensionPath, '..', '..', 'target', 'debug', BINARY));
    const found = candidates.find(isFile);
    if (found) {
        return found;
    }
    throw new Error(
        'The wasm-dap executable was not found. Install it with ' +
        '`cargo install --git https://github.com/yusuf-toktas/wasm-dap wasm-dap-cli`, ' +
        'add it to PATH, or set "wasm-dap.adapterPath".'
    );
}

class AdapterFactory {
    constructor(context) {
        this.context = context;
    }

    createDebugAdapterDescriptor() {
        const executable = resolveAdapter(this.context);
        const level = vscode.workspace.getConfiguration('wasm-dap').get('logLevel') || 'warn';
        return new vscode.DebugAdapterExecutable(executable, ['serve', '--log', level]);
    }
}

class ConfigurationProvider {
    provideDebugConfigurations() {
        return [
            {
                type: DEBUG_TYPE,
                request: 'launch',
                name: 'Debug WebAssembly',
                program: '${workspaceFolder}/target/wasm32-wasip1/debug/${workspaceFolderBasename}.wasm',
                args: [],
                cwd: '${workspaceFolder}',
            },
        ];
    }

    resolveDebugConfiguration(folder, config) {
        if (!config.type && !config.request && !config.name) {
            const file = vscode.window.activeTextEditor?.document.fileName;
            if (file && /\.(wasm|wat)$/i.test(file)) {
                config = {
                    type: DEBUG_TYPE,
                    request: 'launch',
                    name: `Debug ${path.basename(file)}`,
                    program: file,
                };
            }
        }
        if (!config.program) {
            return vscode.window
                .showErrorMessage('wasm-dap: set "program" to the .wasm or .wat file to debug.')
                .then(() => undefined);
        }
        if (!config.cwd && folder) {
            config.cwd = folder.uri.fsPath;
        }
        return config;
    }
}

function describeModule(context, file) {
    return new Promise((resolve) => {
        let executable;
        try {
            executable = resolveAdapter(context);
        } catch {
            resolve(undefined);
            return;
        }
        execFile(executable, ['info', '--json', file], { maxBuffer: 64 * 1024 * 1024 }, (error, stdout) => {
            if (error) {
                resolve(undefined);
                return;
            }
            try {
                resolve(JSON.parse(stdout));
            } catch {
                resolve(undefined);
            }
        });
    });
}

/** Starts a session for a module, asking which export to call when it has no entry point. */
async function debugFile(context, uri) {
    if (!vscode.workspace.isTrusted) {
        vscode.window.showErrorMessage('wasm-dap: debugging requires a trusted workspace.');
        return;
    }
    const target = uri ?? vscode.window.activeTextEditor?.document.uri;
    if (!target || !/\.(wasm|wat)$/i.test(target.fsPath)) {
        vscode.window.showErrorMessage('wasm-dap: select a .wasm or .wat file to debug.');
        return;
    }
    const file = target.fsPath;
    const config = {
        type: DEBUG_TYPE,
        request: 'launch',
        name: `Debug ${path.basename(file)}`,
        program: file,
    };
    const report = await describeModule(context, file);
    if (report) {
        const exports = report.exports.filter((e) => e.kind === 'func').map((e) => e.name);
        const hasEntry = exports.includes('_start') || report.start !== null;
        if (!hasEntry) {
            if (exports.length === 0) {
                vscode.window.showErrorMessage('wasm-dap: the module exports no functions to call.');
                return;
            }
            const choice = await vscode.window.showQuickPick(exports, {
                placeHolder: 'The module has no _start; choose the exported function to call',
            });
            if (!choice) {
                return;
            }
            const signature = report.functions.find(
                (f) => report.exports.some((e) => e.name === choice && e.index === f.index)
            )?.signature;
            const params = signature ? signature.slice(1, signature.indexOf(')')).trim() : '';
            let args = [];
            if (params.length > 0) {
                const input = await vscode.window.showInputBox({
                    prompt: `Arguments for ${choice}${signature}`,
                    placeHolder: 'comma-separated, e.g. 1, 2',
                });
                if (input === undefined) {
                    return;
                }
                args = input.split(',').map((s) => s.trim()).filter((s) => s.length > 0);
            }
            config.invoke = choice;
            config.invokeArgs = args;
        }
    }
    const folder = vscode.workspace.getWorkspaceFolder(target);
    await vscode.debug.startDebugging(folder, config);
}

function activate(context) {
    context.subscriptions.push(
        vscode.debug.registerDebugAdapterDescriptorFactory(DEBUG_TYPE, new AdapterFactory(context)),
        vscode.debug.registerDebugConfigurationProvider(DEBUG_TYPE, new ConfigurationProvider()),
        vscode.commands.registerCommand('wasm-dap.debugFile', (uri) => debugFile(context, uri))
    );
}

function deactivate() {}

module.exports = { activate, deactivate, resolveAdapter };
