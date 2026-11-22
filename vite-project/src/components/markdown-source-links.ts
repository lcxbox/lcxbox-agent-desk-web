// Only these opaque markers become internal history links. Markdown URLs,
// code samples and existing links keep their existing rendering policy.
type Node = { type: string; value?: string; children?: Node[]; url?: string; data?: unknown };
// Database-backed source ids: table letter, row key and check letter.
export const agentSourceIdPattern = /^src_[hm][1-9][0-9]{0,18}[a-j]$/;

export function agentSourceHref(sourceId: string, revision?: number) {
    const query = new URLSearchParams({ source: sourceId });
    if (revision != null) query.set('revision', String(revision));
    return `/ai-agent/conversation?${query}`;
}

export function remarkAgentSources(options: { sources: readonly { source_id: string; source_revision: number }[]; sourceLabel: string; unavailableLabel: string }) {
    return (tree: Node) => {
        const visit = (node: Node) => {
            if (!node.children || ['code', 'inlineCode', 'link', 'linkReference'].includes(node.type)) return;
            node.children = node.children.flatMap(child => {
                if (child.type !== 'text' || !child.value) { visit(child); return [child]; }
                const output: Node[] = [];
                let offset = 0;
                for (const match of child.value.matchAll(/〔来源:([^〕\n]*)(?:〕|$)/g)) {
                    const start = match.index!;
                    if (start > offset) output.push({ type: 'text', value: child.value.slice(offset, start) });
                    const index = agentSourceIdPattern.test(match[1]) ? options.sources.findIndex(source => source.source_id === match[1] && Number.isSafeInteger(source.source_revision) && source.source_revision > 0) : -1;
                    if (index >= 0 && match[0].endsWith('〕')) {
                        output.push({ type: 'link', url: agentSourceHref(match[1], options.sources[index].source_revision),
                            data: { hProperties: { 'data-agent-source': match[1], 'data-agent-source-revision': options.sources[index].source_revision } },
                            children: [{ type: 'text', value: `〔${options.sourceLabel} ${index + 1}〕` }] });
                    } else output.push({ type: 'text', value: `〔${options.unavailableLabel}〕` });
                    offset = start + match[0].length;
                }
                if (offset < child.value.length) output.push({ type: 'text', value: child.value.slice(offset) });
                return output;
            });
        };
        visit(tree);
    };
}
