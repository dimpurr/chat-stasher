import { readFileSync } from 'node:fs';
import Ajv2020 from 'ajv/dist/2020.js';
import { expect, it } from 'vitest';
import { isInboxBundle, validateInboxBundle } from '../lib/contract';

// JSON Schema proves structure; runtime validation additionally proves exact
// decoded bytes, canonical encoding, half-open range and SHA-256 equality.
const corpus = new URL('../../../contracts/fixtures/inbox/', import.meta.url);
const schema = JSON.parse(readFileSync(new URL('../../../contracts/inbox.schema.json', import.meta.url), 'utf8'));
const validate = new Ajv2020({ strict: false, validateFormats: false }).compile(schema);
const cases: { file: string; valid: boolean; schemaValid?: boolean }[] = JSON.parse(
  readFileSync(new URL('manifest.json', corpus), 'utf8'),
);
for (const fixture of cases) {
  it(`schema and TypeScript agree: ${fixture.file}`, async () => {
    const bundle = JSON.parse(readFileSync(new URL(fixture.file, corpus), 'utf8'));
    const structural = fixture.schemaValid ?? fixture.valid;
    expect(validate(bundle)).toBe(structural);
    expect(isInboxBundle(bundle)).toBe(structural);
    expect(await validateInboxBundle(bundle)).toBe(fixture.valid);
  });
}
