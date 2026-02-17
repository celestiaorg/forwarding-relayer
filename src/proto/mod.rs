pub mod celestia {
    pub mod forwarding {
        #[allow(dead_code)]
        pub mod v1 {
            include!("celestia.forwarding.v1.rs");
        }
    }
}

pub mod cosmos {
    pub mod base {
        #[allow(dead_code)]
        pub mod v1beta1 {
            include!("cosmos.base.v1beta1.rs");
        }
    }
}
