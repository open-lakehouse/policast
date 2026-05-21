# The Research
> Note: This research is based off of an unbiased dive into various governance techniques employed across the three top open-source data and ai catalogs.
> 
> It looks at Unity Catalog, Apache Polaris, and Lakekeeper, and provides a lens based on the authors (Scott Haines)
> personal experience with composable systems and more importantly distributed systems tooling that enable code generation (like gRPC/connectRPC)
>

# Governance is hard
Let me be clear, we are here because data+ai governance is difficult. When we set rules, people get upset. The same can be said of our automations, when we `block` something from happening, feelings will get hurt (agents get mad). 

Now, one of the things that simplifies the surface area of this particular problem is having a clear set of compile-time policies that can be treated "as-code". 
This is what led me to start to connect dots across a fairly large swath of open-source projects. I spent a lot of time in the protobuf ecosystem, and that
led me to do a lot of work then in the gRPC ecosystem. This was a lightbulb moment for me personally, "look what we can build when we have primatives that do 
the right thing without us (humans) needing to get in the way".

## Inspiration from the Protobuf ecosystem
I had the pleasure of working with, and then at [Buf.Build](https://buf.build/), and learning from amazing engineers like [Akshay Shah](https://www.linkedin.com/in/akshayjshah/) who created [connectrpc](https://connectrpc.com/) and helped expand the [protovalidate](https://github.com/bufbuild/protovalidate) ecosystem. The thing these projects have in common is a set of primatives that extend from compile-time guarenttees, introduce novel utilities that reduce friction, and simply "just work". This experience also introduced me to Google's [common expression language](https://cel.dev/) a.k.a (cel). The confluence of these ideas together
led me to think about protobuf, grpc/connectrpc, and protovalidate as a set of concrete ingrediants for crafting incredibly reliable distributed systems.

But what about governance? Glad we got here.

## Compile-Time guarenttees, codegen, and governance
The [Cedar Policy Language](https://cedarpolicy.com/) was introduced to me at my time at Nike (the shoe company). We had been struggling with various governance solutions for a very large enterprise. The notion that **cedar** could act as a primitive like IAM policies immediately made sense. Given Amazon was behind the project, it made sense. Not to mention, cedar has a native rust compiler and guarenttees for composable policies - See [SMT Invariants](./smt-invariants.md) for more there, and had this notion of [partial evaluation](https://cedarland.blog/usage/partial-evaluation/content.html). This got the gears spinning.

What if this was the foundation for policy portability? This left the final step of policies + FBAC (row-filters, column masks). The governance space has been trying to figure out how to exchange polices across engines from `catalog's`. How do we represent "functions" and how do we apply these transformations with respect to governance?

## What about Cedar x CEL?
This is what this project set out to discover. Can we create a simplified way of exchanging set's of policies
and additional function primatives, in a way that ensures that preserves the following: 

1. expressions can be composed
2. polices can be stacked or composed
3. invariants across the underlying language will be respected
4. can work across engines (Datafusion, Apache Spark, etc) in a performant way

