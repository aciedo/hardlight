use hardlight::*;
use tracing::info;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let config = ServerConfig::new_self_signed("localhost:8080");
    let server = Server::new(config, factory!(Handler));
    tokio::spawn(async move { server.run().await.unwrap() });

    let mut client = PetServiceClient::new_self_signed(
        "localhost:8080",
        Compression::default(),
    );
    client.connect().await.unwrap();

    client
        .become_dog("Rex".to_string(), 3, "German Shepherd".to_string())
        .await
        .unwrap();
    info!("{:?}", client.state().await.unwrap());
    info!("{}", client.make_a_noise().await.unwrap().unwrap());

    client
        .become_cat("Mittens".to_string(), 2, "Tabby".to_string())
        .await
        .unwrap();
    info!("{:?}", client.state().await.unwrap());
    info!("{}", client.make_a_noise().await.unwrap().unwrap());
}

#[rpc]
trait PetService {
    async fn become_dog(
        &self,
        name: String,
        age: u8,
        breed: String,
    ) -> HandlerResult<()>;
    async fn become_cat(
        &self,
        name: String,
        age: u8,
        breed: String,
    ) -> HandlerResult<()>;
    async fn make_a_noise(&self) -> HandlerResult<Option<String>>;
}

#[connection_state]
struct State {
    pet: Option<Pet>,
}

#[codable]
#[derive(Debug, PartialEq, Eq, Clone)]
enum Pet {
    Dog {
        name: String,
        age: u8,
        breed: String,
    },
    Cat {
        name: String,
        age: u8,
        breed: String,
    },
}

#[rpc_handler]
impl PetService for Handler {
    async fn become_dog(
        &self,
        name: String,
        age: u8,
        breed: String,
    ) -> HandlerResult<()> {
        let mut state = self.state.write().await;
        state.pet = Some(Pet::Dog { name, age, breed });
        Ok(())
    }

    async fn become_cat(
        &self,
        name: String,
        age: u8,
        breed: String,
    ) -> HandlerResult<()> {
        let mut state = self.state.write().await;
        state.pet = Some(Pet::Cat { name, age, breed });
        Ok(())
    }

    async fn make_a_noise(&self) -> HandlerResult<Option<String>> {
        let state = self.state.read().await;
        match &state.pet {
            Some(Pet::Dog { name, .. }) => {
                Ok(Some(format!("{} says woof!", name)))
            }
            Some(Pet::Cat { name, .. }) => {
                Ok(Some(format!("{} says meow!", name)))
            }
            None => Ok(None),
        }
    }
}
