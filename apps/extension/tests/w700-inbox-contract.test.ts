import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { expect, it } from 'vitest';
import { isInboxBundle, validateInboxBundle } from '../lib/contract';
const root = new URL('../../../contracts/fixtures/inbox/', import.meta.url);
const cases = JSON.parse(readFileSync(new URL('manifest.json', root), 'utf8'));
for (const fixture of cases) {
  it(fixture.file, async () => {
    const bytes = readFileSync(new URL(fixture.file, root));
    expect(createHash('sha256').update(bytes).digest('hex')).toBe(fixture.sha256);
    expect(isInboxBundle(JSON.parse(bytes.toString()))).toBe(fixture.schemaValid ?? fixture.valid);
    expect(await validateInboxBundle(JSON.parse(bytes.toString()))).toBe(fixture.valid);
  });
}
