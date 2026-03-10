import { renderTitle } from '../src/index';\nif (renderTitle() !== '<h1>Hello</h1>') { throw new Error('snapshot mismatch'); }
