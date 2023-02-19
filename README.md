<p align="center">
<img width="500" height="500" src="resources/logo.png">
</p>
<p align="center">
A rusty, efficient Byzantine fault tolerant middleware library/framework.
<!-- TODO: include crates.io, docs.rs links here, etc -->
</p>

---

# A bit of context

FeBFT is an efficient BFT SMR middleware library implementation, directly descendant
of protocols such as PBFT and BFT-SMaRt, where a static group of `n = 3f + 1` nodes
are responsible for replicating a service, that is usually exposed via a RPC interface.
The properties of these systems are such that despite the failure of (up to) `f` nodes
(due to software bugs, power outages, malicious attackers, etc) the service abstraction
will continue operating as usual.

Different from prior art in this field, usually implemented in Java, Rust was
the language of choice to implement all the typical SMR sub-protocols present
in FeBFT. Many people are (rightfully so!) excited about the use of Rust
in new (and even older) software, because of its safety properties associated
with the many compile time checks present in the compiler, that are able to
hunt down common use-after-free as well as concurrency related bugs.

There are infinitely many use cases for BFT systems, which will undoubtedly improve the
availability of a digital service. However, a less robust class of systems, called CFT
systems, are often utilized in place of BFT systems, based on their greater performance.
Despite this, with the evolution of hardware, and especially the growing popularity of
blockchain technology, BFT systems are becoming more attractive to distributed system
developers.

People who are interested in the inner workings of these protocols can
consult the following papers:

* Castro, Miguel, and Barbara Liskov. "Practical Byzantine fault tolerance and proactive recovery." ACM Transactions on Computer Systems (TOCS) 20.4 (2002): 398-461.
* Bessani, Alysson, Joao Sousa, and Eduardo EP Alchieri. "State machine replication for the masses with BFT-SMART." 2014 44th Annual IEEE/IFIP International Conference on Dependable Systems and Networks. IEEE, 2014.

<!-- TODO: include link to thesis 
To read more about the architecture of FeBFT, you can check out my MsC thesis.-->

# How to use this library?

Generally, to use this library, you will need to implement the following trait:

```rust
pub trait Service {
    /// The data types used by the application and the SMR protocol.
    ///
    /// This includes their respective serialization routines.
    type Data: SharedData;

    /// Returns the initial state of the application.
    fn initial_state(&mut self) -> Result<State<Self>>;

    /// Process a user request, producing a matching reply,
    /// meanwhile updating the application state.
    fn update(
        &mut self,
        state: &mut State<Self>,
        request: Request<Self>,
    ) -> Reply<Self>;
}
```

You may want to check out [client-local.rs](examples/client-local.rs) and
[replica-local.rs](examples/replica-local.rs) for examples of how to write
services utilizing FeBFT. Run them with:

```
# Start the service replicas in a terminal window
$ cargo run --release --example replica-local

# In another terminal window, start the client(s)
$ cargo run --release --example client-local
```

# For contributors

The code is organized as follows:

* `src/bft/core/client` is the main entry point for a client.
* `src/bft/core/server` is the main entry point for a replica.
* `src/bft/consensus` implements the normal phase consensus code.
* `src/bft/consensus/log` implements the message log.
* `src/bft/sync` implements the view change code.
* `src/bft/cst` implements the state transfer code.
* `src/bft/communication` and its sub-modules implement the network code.
* `src/bft/executable` implements the thread responsible for running the
  user's application code.
* `src/bft/ordering` defines code for sequence numbers resistant to overflows.

Other modules should be somewhat self explanatory, especially if you read
the documentation generated with `cargo doc --features expose_impl` for FeBFT.

# Quick glimpse on performance

We will now take a quick look at the performance versus another similar BFT SMR system, BFT-SMaRt. The test we ran was the microbenchmarks asynchronous test, meant to test the peak performance of the system. 

## Operations per second

![ops_per_second_side_by_side_async](https://user-images.githubusercontent.com/4153112/201152436-7ea6eedb-0c48-4a00-96bb-dab625dfaa79.png)

In this image we are able to see the operations per second of both BFT-SMaRt (left) and FeBFT (right). We can see that FeBFT's performance is much more stable and actually higher. This is due to many architectural factors in FeBFT, which were thought out in order to maximize performance and scalability, as well as factors related to the choice of language to implement this protocol.

The average performance for FeBFT is 111552 +/- 25000. This average includes some lackluster measurements including the initialization and final steps of the program which have a lower performance than the real peak we want to test. As such the 95th percentile average is a better demonstration, in which we get 121914 operations/sec.
BFT-SMaRt's performance averages at around 43229 +/- 28068. Again similarly to FeBFT, we took the 95th percentile average as we believe it to be the more accurate representation which is 98296 ops/sec.

## RAM Usage

![ram_usage_side_by_side_async](https://user-images.githubusercontent.com/4153112/201156651-c86c8266-f397-4b1f-95c0-7e2225674e8d.png)

In this image we can see the evolution of the utilization of RAM by the system in the leader replica. The graph is represented in Bytes and the scale is 10^10, which means that in the graph the number 1 in the Y axis means we are using 10GB of RAM. BFT-SMaRt's performance is on the left while FeBFT's performance is on the right.

We can clearly see the much more uncontrolled rise in RAM usage of BFT-SMaRt which then gets controlled by the garbage collector. Comparing this with FeBFT which automatically cleans up it's own memory without the need for a garbage collector we can see a very large difference. The rise in usage of RAM by FeBFT is due to it storing all of the messages in the log in RAM at the moment and the test not having enough operations in order to trigger a checkpoint, which would then allow FeBFT to dispose of its log and just keep the checkpoint instead.
Since BFT-SMaRt does not clean up after itself, in this test where we have many many requests being sent, the garbage collector needs to be called very often and has a lot of work to do, leading to a lot of time where no thread is able to make advancements since they are all waiting to the GC to terminate. This leads to poor and unstable performance when compared with FeBFT.

The graph is also a bit misleading since even though it seems FeBFT's RAM usage rises similarly to BFT-SMaRt's which is not at all true. In reality, FeBFT reached 12 GB of RAM used at the end of the test (it's peak) while BFT-SMaRt's peak memory usage is of 40GB (however that was before it was garbage collected).

### For more information about FeBFT, please visit the wiki here: https://github.com/SecureSolutionsLab/febft/wiki .
