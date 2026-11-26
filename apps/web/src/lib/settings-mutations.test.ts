import { describe, expect, test } from 'bun:test'
import { createSettingsMutator } from './settings-mutations'

type Account = { id: string }
type Settings = { provider_accounts: Account[] }

function deferred<T = void>() {
  let resolve: (value: T) => void = () => {}
  const promise = new Promise<T>((done) => {
    resolve = done
  })
  return { promise, resolve }
}

describe('createSettingsMutator', () => {
  test('overlapping deletes both stick and are not based on the pre-delete snapshot', async () => {
    const initial: Settings = {
      provider_accounts: [{ id: 'a' }, { id: 'b' }, { id: 'c' }],
    }
    let stored = structuredClone(initial)
    let reads = 0
    const writes: string[][] = []
    const firstWrite = deferred<void>()
    const apply = createSettingsMutator(
      async () => {
        reads += 1
        // Always the snapshot from before either delete. A mutator that
        // re-reads this between saves would restore the account the other
        // request removed.
        return structuredClone(initial)
      },
      async (next) => {
        writes.push(next.provider_accounts.map((account) => account.id))
        if (writes.length === 1) await firstWrite.promise
        stored = next
      },
    )

    const first = apply((current) => ({
      ...current,
      provider_accounts: current.provider_accounts.filter((account) => account.id !== 'a'),
    }))
    const second = apply((current) => ({
      ...current,
      provider_accounts: current.provider_accounts.filter((account) => account.id !== 'b'),
    }))
    firstWrite.resolve()
    await first
    await second

    expect(writes).toEqual([['b', 'c'], ['c']])
    expect(stored.provider_accounts.map((account) => account.id)).toEqual(['c'])
    expect(reads).toBe(1)
  })
})
