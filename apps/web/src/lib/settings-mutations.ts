/** Serialize account saves. Each save is based on the previous in-flight
 * result, not a snapshot captured before that save. A failed write is not the
 * base of the next one and does not poison the queue. */
export function createSettingsMutator<T>(
  read: () => T | Promise<T>,
  write: (next: T) => Promise<void>,
) {
  let tail: Promise<T | undefined> = Promise.resolve(undefined)
  return (mutate: (current: T) => T | Promise<T>): Promise<T> => {
    const previousTail = tail
    const operation = previousTail.then(
      (previous) => previous,
      () => undefined,
    ).then(async (previous) => {
      const current = previous ?? await read()
      const next = await mutate(current)
      await write(next)
      return next
    })
    tail = operation
    return operation
  }
}
