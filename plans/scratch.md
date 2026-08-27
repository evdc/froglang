See @README.md . This is a design brainstorm. Look over froglang's codebase and design docs, then evaluate this proposed change/fork. Synthesize ideas, evalaute trade-offs, make proposals. froglang is a toy language, unreleased and in development, so broad sweeping changes are entirely within scope. Plan to go back and forth before committing to any implementation. (The outcome may be, in fact, a different language project which borrows from froglang some semantics and some Rust implementation code.)

I am considering reorienting to make *tables* the primary means of structured data. A table has rows and named (typed) columns, like a SQL table or a data frame. Tables will (semantically) subsume lists (a list is a table with one value column, implicitly perhaps called `val`, and one implicit primary key column, which is the dense integer index); records/structs (a record is a table with one row, and n columns, which are its fields); maps (a map is a table with one key column of type K and one value column of type V). 

Table query expressions then generalize list indexing / comprehensions, and add a kind of relational feel to the language:
```
type Person { name: Str, age: Int }

table people: [Person] = [
    "alice", 34;
    "bob", 42;
    "carol", 17
]

people[.age > 18].name  // ["alice", "bob"] 

people { .name, new_age = .age + 1 }    // [{name: "alice", new_age: 35}, ...]

people.groupby(.name.len()) { .key, count(.) }  //  [{key: 5, count: 2}, {key: 3, count: 1}]

people insert {name: "dave", age: 55}

people[.age > 18] <- {.name = .name.upper()}    // inplace update
```

As this is a programming language and not actually a principled rdbms, tables can be nested, just as structs can be nested.
For example, this enables `.groupby()` to produce a table of shape `{K: {G}}` - a table mapping group keys, to group values, which are themselves tables. An aggregate like "count each group" is then a regular function `count :: {...} -> Int` applied to each row of the table-of-groups. 

Every table has an implicit `id` primary key column unless explicitly overridden, row ids are passed around instead of passing whole-rows-as-values around?

Things I'd want to keep: errors-as-values, unions, simple/minimal syntax that gets out of the way (e.g. for error handling). 
A union, e.g. `Shape`, is represented as a table for each non-nullary variant, plus an overall union table with columns (id, variant_tag, key_into_whichever_variant_table). This pattern can be annoying in SQL because you have a sort of tag-dependent FK which most DBs can't really enforce but we can make the compiler/runtime handle this case.

There may be (if useful, if not too full of footguns) a scalar-promotion and auto-broadcast rule, e.g. `people.age * 2` Just Works, and `people[.name = "alice"].age` is a scalar `34` (you don't need to unwrap or `.get()` on it or w/e) but can also be treated as a single-row-single-col table where it makes sense to do so?

Sketch some examples of syntax and semantics, explore corner cases, going from high-level "how does this look and feel" to concrete theoretical rigor and pragmatic performance considerations.

See also some prior art in this direction in the adjacent repos at `../ripple` and `../rex` . Those projects orient around "compile relational ops to DBS circuits"; this language *might* be able to work in that direction, but may as well just be a quick get-things-done tool which evaluates relational expressions directly, eagerly, against tables in memory. (The language/compiler may choose to represent tables in row-major or column-major/SoA/ECS-style layout; possibly guided by annotations)

---

