'use strict';

// Integration scenario executed inside the VS Code extension host: set a
// breakpoint in a .wat file, debug it through the extension, inspect the
// paused program and let it run to completion.

const assert = require('assert');
const path = require('path');
const vscode = require('vscode');

function waitFor(predicate, description, timeoutMs = 60000) {
    return new Promise((resolve, reject) => {
        const started = Date.now();
        const timer = setInterval(() => {
            const value = predicate();
            if (value) {
                clearInterval(timer);
                resolve(value);
            } else if (Date.now() - started > timeoutMs) {
                clearInterval(timer);
                reject(new Error(`timed out waiting for ${description}`));
            }
        }, 25);
    });
}

async function run() {
    const folder = vscode.workspace.workspaceFolders[0];
    const file = path.join(folder.uri.fsPath, 'counter.wat');
    const document = await vscode.workspace.openTextDocument(file);
    await vscode.window.showTextDocument(document);
    assert.strictEqual(document.languageId, 'wat', 'the extension registers the WAT language');

    const lines = document.getText().split(/\r?\n/);
    const squareLine = lines.findIndex((l) => l.trim().startsWith('i32.mul'));
    assert.ok(squareLine > 0);
    vscode.debug.addBreakpoints([
        new vscode.SourceBreakpoint(new vscode.Location(document.uri, new vscode.Position(squareLine, 0))),
    ]);

    const messages = [];
    const tracker = vscode.debug.registerDebugAdapterTrackerFactory('wasm-dap', {
        createDebugAdapterTracker() {
            return { onDidSendMessage: (message) => messages.push(message) };
        },
    });
    let terminated = false;
    const terminateListener = vscode.debug.onDidTerminateDebugSession(() => {
        terminated = true;
    });

    const started = await vscode.debug.startDebugging(folder, {
        type: 'wasm-dap',
        request: 'launch',
        name: 'counter',
        program: file,
        invoke: 'sum_of_squares',
        invokeArgs: [3, 4],
    });
    assert.ok(started, 'debug session started');
    const session = await waitFor(() => vscode.debug.activeDebugSession, 'an active session');

    const stoppedEvents = () => messages.filter((m) => m.type === 'event' && m.event === 'stopped');
    await waitFor(() => stoppedEvents().length >= 1, 'the first stop');
    assert.strictEqual(stoppedEvents()[0].body.reason, 'breakpoint');

    const trace = await session.customRequest('stackTrace', { threadId: 1 });
    const [top, caller] = trace.stackFrames;
    assert.strictEqual(top.name, 'square');
    assert.strictEqual(top.line, squareLine + 1);
    assert.ok(top.source.path.toLowerCase().endsWith('counter.wat'), top.source.path);
    assert.strictEqual(caller.name, 'sum_of_squares');

    const x = await session.customRequest('evaluate', { expression: '$x', frameId: top.id, context: 'watch' });
    assert.strictEqual(x.result, '3');
    const a = await session.customRequest('evaluate', { expression: '$a * 10 + $b', frameId: caller.id, context: 'watch' });
    assert.strictEqual(a.result, '34');
    const scopes = await session.customRequest('scopes', { frameId: top.id });
    assert.ok(scopes.scopes.some((s) => s.name === 'Operand Stack'));

    await session.customRequest('continue', { threadId: 1 });
    await waitFor(() => stoppedEvents().length >= 2, 'the second stop');
    const second = await session.customRequest('stackTrace', { threadId: 1 });
    const value = await session.customRequest('evaluate', {
        expression: '$x',
        frameId: second.stackFrames[0].id,
        context: 'watch',
    });
    assert.strictEqual(value.result, '4');

    vscode.debug.removeBreakpoints(vscode.debug.breakpoints);
    await session.customRequest('continue', { threadId: 1 });
    await waitFor(() => terminated, 'the session to end');

    const output = messages
        .filter((m) => m.type === 'event' && m.event === 'output')
        .map((m) => m.body.output)
        .join('');
    assert.ok(output.includes('sum_of_squares(3, 4) = 25'), output);
    const exited = messages.find((m) => m.type === 'event' && m.event === 'exited');
    assert.strictEqual(exited.body.exitCode, 0);

    tracker.dispose();
    terminateListener.dispose();
}

module.exports = { run };
