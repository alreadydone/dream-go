// Copyright 2017 Karl Sundequist Blomdahl <karl.sundequist.blomdahl@gmail.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

extern crate dream_go;
extern crate time;

use dream_go::{gtp, nn, mcts};
use dream_go::util::config::{self, Procedure};

/// Returns the network weights, panics if it failed to load the weights.
fn load_network() -> nn::Network {
    match nn::Network::new() {
        Some(network) => network,
        None => {
            println!("Could not load network weights!");
            ::std::process::exit(1);
        }
    }
}

/// Main function.
fn main() {
    match *config::PROCEDURE {
        Procedure::Help => {
            println!("Usage: ./dream-go [options]");
            println!();
            println!("  --self-play <n>       Extract a dataset from self-play containing n examples");
            println!("  --policy-play <n>     Extract a dataset from self-play using only the policy network");
            println!("  --ex-it               When combined with --policy-play perform search on some partial");
            println!("                        policies");
            println!("  --gtp                 Run GTP client (default)");
            println!();
            println!("Advanced options:");
            println!("  --num-rollout <n>     The number of rollouts to add to the search tree for every move");
            println!("  --num-games <n>       The number of games to play or extract in parallel");
            println!("  --num-threads <n>     The number of search threads to use in total");
            println!("  --num-samples <n>     The number of games to extract from each game record");
            println!("  --batch-size <n>      The number parallel rollouts to perform on the GPU");
            println!("  --no-ponder           Do not think in the background during idle time");
            println!("  --with-sabaki         Include Sabaki extensions amongst the GTP commands");
        },

        Procedure::SelfPlay(n) => {
            let (receiver, _server) = mcts::self_play(load_network(), n);

            for result in receiver.iter() {
                println!("{}", result);
            }
        },

        Procedure::PolicyPlay(n, ex_it) => {
            let (receiver, _server) = mcts::policy_play(load_network(), n, ex_it);

            for result in receiver.iter() {
                println!("{}", result);
            }
        },

        Procedure::Gtp => {
            gtp::run()
        }
    }
}
