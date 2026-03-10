function wrap<T extends string>(value: T): T {\n  return value;\n}\nexport const out = wrap(42);
