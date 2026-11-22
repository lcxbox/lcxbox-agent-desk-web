import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

describe('agent page header layout', () => {
    it('keeps device identity, connection state and conversation actions together', () => {
        const source = readFileSync('src/features/desk/ai-agent-page.tsx', 'utf8');
        expect(source).not.toContain('data-testid="agent-signal-status"');
        expect(source.match(/<AgentConnectionIcon /g)).toHaveLength(1);
        expect(source).toMatch(/<CardTitle[^>]*>\s*<AgentConnectionIcon connected=\{isConnected\} enabled=\{agentEnabled\}/);
        expect(source).toContain('data-testid="agent-title-row" className="flex items-center justify-between gap-2"');
        const header = source.slice(source.indexOf('<CardHeader className="agent-header'), source.indexOf('</CardHeader>', source.indexOf('<CardHeader className="agent-header')));
        expect(header).toContain('<AgentHistory ');
        expect(header).not.toContain('<AgentComposerMenu');
        expect(header).not.toContain('<AgentSubagentMenu');
        expect(header).not.toContain('<CardDescription>');
    });
});


it('keeps the context trigger below the input beside send or stop', () => {
    const page = readFileSync('src/features/desk/ai-agent-page.tsx', 'utf8');
    const composer = page.slice(page.indexOf('<form onSubmit={submit} className="agent-composer'), page.indexOf('</form>', page.indexOf('<form onSubmit={submit} className="agent-composer')));
    const input = composer.indexOf('<AgentComposerInput');
    const actions = composer.indexOf('<div className="flex min-w-0 items-center gap-1">');
    const add = composer.indexOf('<AgentComposerMenu');
    const status = composer.indexOf('<AgentComposerStatus');
    const statusEnd = composer.indexOf('/>', status);
    const send = composer.indexOf('<AgentComposerActions ');
    expect(input).toBeGreaterThanOrEqual(0);
    expect(actions).toBeGreaterThan(input);
    expect(add).toBeGreaterThan(actions);
    expect(composer).toContain('sections={menuSections}');
    expect(status).toBeGreaterThan(add);
    expect(composer.slice(status, statusEnd)).toContain('automaticApproval={automaticApproval}');
    expect(composer.slice(status, statusEnd)).toContain('contextCount={selectedContextCount}');
    expect(send).toBeGreaterThan(statusEnd);
    const main = page.slice(page.indexOf('id="agent-main-panel"'), page.indexOf('data-testid="agent-scroll-area"'));
    expect(main).not.toContain('approvalDelegation');
    expect(main).not.toContain('<AgentComposerStatus');
});

it('keeps standalone back navigation beside the conversation heading without the outer title or width cap', () => {
    const page = readFileSync('src/features/desk/ai-agent-page.tsx', 'utf8');
    const standalone = page.slice(page.indexOf('function AiAgentLivePage'));
    expect(standalone).not.toContain("t('pages.aiAgent.title')");
    expect(standalone).not.toContain("t('pages.aiAgent.subtitle')");
    expect(page).not.toContain('max-w-4xl');
    expect(standalone).toContain('absolute inset-0 flex min-w-0 flex-col gap-2 overflow-hidden');
    expect(standalone.match(/backTo=\{/g)).toHaveLength(1);
    const header = page.slice(page.indexOf('data-testid="agent-title-row"'), page.indexOf('</CardHeader>', page.indexOf('data-testid="agent-title-row"')));
    expect(header).toContain('{backTo && (');
    expect(header).toContain("aria-label={t(hasDevice ? 'pages.aiAgent.backToDevice' : 'pages.aiAgent.backToOverview')}");
    expect(header.indexOf('<Link to={backTo}')).toBeLessThan(header.indexOf('<CardTitle'));
});
