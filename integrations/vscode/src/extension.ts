import * as vscode from 'vscode';
import { HeliosDBClient } from './client';
import { ConnectionTreeProvider } from './views/connections';
import { ExplorerTreeProvider } from './views/explorer';
import { BranchesTreeProvider } from './views/branches';
import { VectorStoresTreeProvider } from './views/vectors';
import { HistoryTreeProvider } from './views/history';
import { QueryResultsPanel } from './panels/results';
import { VectorSearchPanel } from './panels/vectorSearch';
import { NLQueryPanel } from './panels/nlQuery';

let client: HeliosDBClient | undefined;
let statusBarItem: vscode.StatusBarItem;

export function activate(context: vscode.ExtensionContext) {
    console.log('HeliosDB extension is now active!');

    // Create status bar item
    statusBarItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
    statusBarItem.text = '$(database) HeliosDB: Disconnected';
    statusBarItem.command = 'heliosdb.connect';
    statusBarItem.show();
    context.subscriptions.push(statusBarItem);

    // Register tree data providers
    const connectionProvider = new ConnectionTreeProvider(context);
    const explorerProvider = new ExplorerTreeProvider();
    const branchesProvider = new BranchesTreeProvider();
    const vectorsProvider = new VectorStoresTreeProvider();
    const historyProvider = new HistoryTreeProvider();

    vscode.window.registerTreeDataProvider('heliosdb-connections', connectionProvider);
    vscode.window.registerTreeDataProvider('heliosdb-explorer', explorerProvider);
    vscode.window.registerTreeDataProvider('heliosdb-branches', branchesProvider);
    vscode.window.registerTreeDataProvider('heliosdb-vectors', vectorsProvider);
    vscode.window.registerTreeDataProvider('heliosdb-history', historyProvider);

    // Register commands
    const commands = [
        vscode.commands.registerCommand('heliosdb.connect', async () => {
            const url = await vscode.window.showInputBox({
                prompt: 'Enter HeliosDB URL',
                placeHolder: 'http://localhost:8080',
                value: 'http://localhost:8080'
            });

            if (!url) return;

            const apiKey = await vscode.window.showInputBox({
                prompt: 'Enter API Key (optional)',
                password: true
            });

            try {
                client = new HeliosDBClient(url, apiKey || undefined);
                await client.connect();

                statusBarItem.text = `$(database) HeliosDB: Connected`;
                statusBarItem.tooltip = url;
                vscode.commands.executeCommand('setContext', 'heliosdb.connected', true);

                // Refresh all views
                connectionProvider.refresh();
                explorerProvider.setClient(client);
                branchesProvider.setClient(client);
                vectorsProvider.setClient(client);

                vscode.window.showInformationMessage('Connected to HeliosDB');
            } catch (error) {
                vscode.window.showErrorMessage(`Failed to connect: ${error}`);
            }
        }),

        vscode.commands.registerCommand('heliosdb.disconnect', async () => {
            client = undefined;
            statusBarItem.text = '$(database) HeliosDB: Disconnected';
            vscode.commands.executeCommand('setContext', 'heliosdb.connected', false);
            vscode.window.showInformationMessage('Disconnected from HeliosDB');
        }),

        vscode.commands.registerCommand('heliosdb.executeQuery', async () => {
            if (!client) {
                vscode.window.showWarningMessage('Not connected to HeliosDB');
                return;
            }

            const editor = vscode.window.activeTextEditor;
            if (!editor) return;

            const selection = editor.selection;
            const sql = selection.isEmpty
                ? editor.document.getText()
                : editor.document.getText(selection);

            if (!sql.trim()) {
                vscode.window.showWarningMessage('No SQL to execute');
                return;
            }

            try {
                const startTime = Date.now();
                const result = await client.query(sql);
                const duration = Date.now() - startTime;

                // Add to history
                historyProvider.addQuery(sql, duration);

                // Show results
                QueryResultsPanel.show(context.extensionUri, result, duration);
            } catch (error) {
                vscode.window.showErrorMessage(`Query failed: ${error}`);
            }
        }),

        vscode.commands.registerCommand('heliosdb.explainQuery', async () => {
            if (!client) {
                vscode.window.showWarningMessage('Not connected to HeliosDB');
                return;
            }

            const editor = vscode.window.activeTextEditor;
            if (!editor) return;

            const selection = editor.selection;
            const sql = selection.isEmpty
                ? editor.document.getText()
                : editor.document.getText(selection);

            try {
                const result = await client.explain(sql);
                QueryResultsPanel.showExplain(context.extensionUri, result);
            } catch (error) {
                vscode.window.showErrorMessage(`Explain failed: ${error}`);
            }
        }),

        vscode.commands.registerCommand('heliosdb.createBranch', async () => {
            if (!client) {
                vscode.window.showWarningMessage('Not connected to HeliosDB');
                return;
            }

            const name = await vscode.window.showInputBox({
                prompt: 'Enter branch name',
                placeHolder: 'feature-branch'
            });

            if (!name) return;

            const fromBranch = await vscode.window.showQuickPick(
                client.listBranches().then(b => b.map(br => br.name)),
                { placeHolder: 'Select source branch' }
            );

            try {
                await client.createBranch(name, fromBranch || 'main');
                branchesProvider.refresh();
                vscode.window.showInformationMessage(`Branch '${name}' created`);
            } catch (error) {
                vscode.window.showErrorMessage(`Failed to create branch: ${error}`);
            }
        }),

        vscode.commands.registerCommand('heliosdb.switchBranch', async (branch?: string) => {
            if (!client) {
                vscode.window.showWarningMessage('Not connected to HeliosDB');
                return;
            }

            const branchName = branch || await vscode.window.showQuickPick(
                client.listBranches().then(b => b.map(br => br.name)),
                { placeHolder: 'Select branch to switch to' }
            );

            if (!branchName) return;

            client.setCurrentBranch(branchName);
            statusBarItem.text = `$(database) HeliosDB: ${branchName}`;
            branchesProvider.refresh();
            explorerProvider.refresh();
            vscode.window.showInformationMessage(`Switched to branch '${branchName}'`);
        }),

        vscode.commands.registerCommand('heliosdb.mergeBranch', async (source?: string) => {
            if (!client) {
                vscode.window.showWarningMessage('Not connected to HeliosDB');
                return;
            }

            const sourceBranch = source || await vscode.window.showQuickPick(
                client.listBranches().then(b => b.map(br => br.name)),
                { placeHolder: 'Select branch to merge' }
            );

            if (!sourceBranch) return;

            const targetBranch = await vscode.window.showQuickPick(
                client.listBranches().then(b => b.map(br => br.name).filter(n => n !== sourceBranch)),
                { placeHolder: 'Select target branch' }
            );

            if (!targetBranch) return;

            try {
                await client.mergeBranch(sourceBranch, targetBranch);
                branchesProvider.refresh();
                vscode.window.showInformationMessage(`Merged '${sourceBranch}' into '${targetBranch}'`);
            } catch (error) {
                vscode.window.showErrorMessage(`Merge failed: ${error}`);
            }
        }),

        vscode.commands.registerCommand('heliosdb.vectorSearch', async (store?: string) => {
            if (!client) {
                vscode.window.showWarningMessage('Not connected to HeliosDB');
                return;
            }

            const storeName = store || await vscode.window.showQuickPick(
                client.listVectorStores().then(s => s.map(vs => vs.name)),
                { placeHolder: 'Select vector store' }
            );

            if (!storeName) return;

            VectorSearchPanel.show(context.extensionUri, client, storeName);
        }),

        vscode.commands.registerCommand('heliosdb.nlQuery', async () => {
            if (!client) {
                vscode.window.showWarningMessage('Not connected to HeliosDB');
                return;
            }

            NLQueryPanel.show(context.extensionUri, client);
        }),

        vscode.commands.registerCommand('heliosdb.refreshExplorer', () => {
            explorerProvider.refresh();
            branchesProvider.refresh();
            vectorsProvider.refresh();
        }),

        vscode.commands.registerCommand('heliosdb.exportData', async () => {
            if (!client) {
                vscode.window.showWarningMessage('Not connected to HeliosDB');
                return;
            }

            const table = await vscode.window.showQuickPick(
                client.listTables().then(t => t.map(tb => tb.name)),
                { placeHolder: 'Select table to export' }
            );

            if (!table) return;

            const format = await vscode.window.showQuickPick(
                ['CSV', 'JSON', 'Parquet'],
                { placeHolder: 'Select export format' }
            );

            if (!format) return;

            const uri = await vscode.window.showSaveDialog({
                filters: {
                    [format]: [format.toLowerCase()]
                }
            });

            if (!uri) return;

            try {
                const data = await client.exportTable(table, format.toLowerCase());
                await vscode.workspace.fs.writeFile(uri, Buffer.from(data));
                vscode.window.showInformationMessage(`Exported ${table} to ${uri.fsPath}`);
            } catch (error) {
                vscode.window.showErrorMessage(`Export failed: ${error}`);
            }
        }),

        vscode.commands.registerCommand('heliosdb.importData', async () => {
            if (!client) {
                vscode.window.showWarningMessage('Not connected to HeliosDB');
                return;
            }

            const uris = await vscode.window.showOpenDialog({
                filters: {
                    'Data Files': ['csv', 'json', 'parquet']
                },
                canSelectMany: false
            });

            if (!uris || uris.length === 0) return;

            const table = await vscode.window.showInputBox({
                prompt: 'Enter table name',
                placeHolder: 'my_table'
            });

            if (!table) return;

            try {
                const data = await vscode.workspace.fs.readFile(uris[0]);
                const ext = uris[0].path.split('.').pop() || 'csv';
                await client.importData(table, data, ext);
                explorerProvider.refresh();
                vscode.window.showInformationMessage(`Imported data to ${table}`);
            } catch (error) {
                vscode.window.showErrorMessage(`Import failed: ${error}`);
            }
        }),

        vscode.commands.registerCommand('heliosdb.showTimeline', async () => {
            if (!client) {
                vscode.window.showWarningMessage('Not connected to HeliosDB');
                return;
            }

            const table = await vscode.window.showQuickPick(
                client.listTables().then(t => t.map(tb => tb.name)),
                { placeHolder: 'Select table to view timeline' }
            );

            if (!table) return;

            // Show timeline panel
            const timeline = await client.getTimeline(table);
            const panel = vscode.window.createWebviewPanel(
                'heliosdbTimeline',
                `Timeline: ${table}`,
                vscode.ViewColumn.One,
                { enableScripts: true }
            );

            panel.webview.html = generateTimelineHtml(timeline);
        })
    ];

    context.subscriptions.push(...commands);

    // Register SQL completion provider
    const completionProvider = vscode.languages.registerCompletionItemProvider(
        ['sql', 'heliosdb-sql'],
        {
            async provideCompletionItems(document, position) {
                if (!client) return [];

                const items: vscode.CompletionItem[] = [];

                // Add table completions
                try {
                    const tables = await client.listTables();
                    for (const table of tables) {
                        const item = new vscode.CompletionItem(table.name, vscode.CompletionItemKind.Class);
                        item.detail = 'Table';
                        items.push(item);
                    }
                } catch (e) {
                    // Ignore
                }

                // Add HeliosDB-specific keywords
                const keywords = [
                    'BRANCH', 'CREATE BRANCH', 'MERGE BRANCH', 'DROP BRANCH',
                    'AS OF TIMESTAMP', 'AS OF VERSION',
                    'VECTOR', 'VECTOR_SEARCH', 'COSINE_DISTANCE', 'EUCLIDEAN_DISTANCE',
                    'CREATE VECTOR INDEX', 'EMBEDDING'
                ];

                for (const keyword of keywords) {
                    const item = new vscode.CompletionItem(keyword, vscode.CompletionItemKind.Keyword);
                    item.detail = 'HeliosDB keyword';
                    items.push(item);
                }

                return items;
            }
        },
        '.'
    );

    context.subscriptions.push(completionProvider);
}

export function deactivate() {
    client = undefined;
}

function generateTimelineHtml(timeline: any[]): string {
    return `<!DOCTYPE html>
<html>
<head>
    <style>
        body { font-family: var(--vscode-font-family); padding: 20px; }
        .timeline { position: relative; padding-left: 30px; }
        .timeline::before {
            content: '';
            position: absolute;
            left: 10px;
            top: 0;
            bottom: 0;
            width: 2px;
            background: var(--vscode-textLink-foreground);
        }
        .event {
            position: relative;
            margin-bottom: 20px;
            padding: 10px;
            background: var(--vscode-editor-background);
            border: 1px solid var(--vscode-widget-border);
            border-radius: 4px;
        }
        .event::before {
            content: '';
            position: absolute;
            left: -24px;
            top: 15px;
            width: 10px;
            height: 10px;
            background: var(--vscode-textLink-foreground);
            border-radius: 50%;
        }
        .event-time { color: var(--vscode-descriptionForeground); font-size: 12px; }
        .event-type { font-weight: bold; }
    </style>
</head>
<body>
    <h2>Table Timeline</h2>
    <div class="timeline">
        ${timeline.map(event => `
            <div class="event">
                <div class="event-time">${event.timestamp}</div>
                <div class="event-type">${event.type}</div>
                <div class="event-details">${JSON.stringify(event.details)}</div>
            </div>
        `).join('')}
    </div>
</body>
</html>`;
}
