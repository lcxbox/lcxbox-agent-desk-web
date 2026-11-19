import { describe, expect, it } from 'vitest'
import { orderPortableImports, portablePluginPaths } from './kubb-portable-paths'
import type { PluginContext, PluginFactoryOptions } from '@kubb/core'

describe('portablePluginPaths', () => {
    it('orders imports identically for Windows and POSIX without rewriting targets', () => {
        const paths = ['@app/client', '/repo/types.ts', '@tanstack/query', '/repo/client.ts']
        const build = (windows: boolean) => paths.map((path, index) => ({
            path: windows && path.startsWith('/') ? `D:${path.replaceAll('/', '\\')}` : path,
            name: [`item${index}`], isTypeOnly: index < 2,
        }))
        const posix = build(false)
        const windows = build(true)
        orderPortableImports(posix)
        orderPortableImports(windows)
        expect(windows.map(entry => entry.name)).toEqual(posix.map(entry => entry.name))
        expect(windows[0].path).toBe('D:\\repo\\types.ts')
        expect(posix.map(entry => entry.name[0])).toEqual(['item1', 'item0', 'item3', 'item2'])
    })
    it('preserves the resolver context and arguments while normalizing Windows paths', () => {
        const context = { config: { root: 'D:\\repo\\services' } }
        const original = {
            name: 'test-paths',
            options: {},
            resolvePath(this: PluginContext<PluginFactoryOptions>, name: string) {
                return `${this.config.root}\\hooks\\${name}`
            },
        }
        // Reflect invokes the public lifecycle hook with Kubb's supplied context.
        const wrapped = portablePluginPaths(original)
        expect(Reflect.apply(wrapped.resolvePath!, context, ['query.ts']))
            .toBe('D:/repo/services/hooks/query.ts')
        expect(Reflect.apply(original.resolvePath, context, ['query.ts']))
            .toBe('D:\\repo\\services\\hooks\\query.ts')
    })

    it('leaves POSIX paths and plugins without a resolver unchanged', () => {
        const withoutResolver = { name: 'no-paths', options: {} }
        expect(portablePluginPaths(withoutResolver)).toBe(withoutResolver)
        const plugin = portablePluginPaths({
            name: 'posix', options: {}, resolvePath: () => '/repo/services/query.ts',
        })
        expect(Reflect.apply(plugin.resolvePath!, {}, [])).toBe('/repo/services/query.ts')
    })
})
