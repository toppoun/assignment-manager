// A sync refresh waits for an active query/toggle instead of being dropped as busy.
export function createListTaskRunner() {
  let current = null;
  return function run(task) {
    if (current) return current.then(() => run(task), () => run(task));
    current = Promise.resolve(task()).finally(() => { current = null; });
    return current;
  };
}
