use hardlight::*;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let config = ServerConfig::new_self_signed("localhost:8080");
    let mut server = PetServiceServer::new(config);
    server.start().await.unwrap();

    let mut client = PetServiceClient::new_self_signed("localhost:8080");
    client.connect().await.unwrap();

    client
        .become_dog("Rex".to_string(), 3, "German Shepherd".to_string())
        .await
        .unwrap();
    println!(
        "Rex says: {}",
        client.make_a_noise().await.unwrap().unwrap()
    );

    client
        .become_cat("Mittens".to_string(), 2, "Tabby".to_string())
        .await
        .unwrap();
    println!(
        "Mittens says: {}",
        client.make_a_noise().await.unwrap().unwrap()
    );
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

#[rpc_handler]
impl PetService for Handler {
    async fn become_dog(
        &self,
        name: String,
        age: u8,
        breed: String,
    ) -> HandlerResult<()> {
        let mut state = self.state.lock();
        state.pet = Some(Pet::Dog(Dog::new(name, age, breed)));
        Ok(())
    }

    async fn become_cat(
        &self,
        name: String,
        age: u8,
        breed: String,
    ) -> HandlerResult<()> {
        let mut state = self.state.lock();
        state.pet = Some(Pet::Cat(Cat::new(name, age, breed)));
        Ok(())
    }

    async fn make_a_noise(&self) -> HandlerResult<Option<String>> {
        let state = self.state.lock();
        match &state.pet {
            Some(Pet::Dog(dog)) => Ok(Some(dog.bark())),
            Some(Pet::Cat(cat)) => Ok(Some(cat.meow())),
            None => Ok(None),
        }
    }
}

#[connection_state]
struct State {
    pet: Option<Pet>,
}

#[codable]
enum Pet {
    Dog(Dog),
    Cat(Cat),
}

#[codable]
struct Dog {
    name: String,
    age: u8,
    breed: String,
}

impl Dog {
    fn new(name: String, age: u8, breed: String) -> Self {
        Self { name, age, breed }
    }

    fn bark(&self) -> String {
        format!("{} says woof!", self.name)
    }
}

#[codable]
struct Cat {
    name: String,
    age: u8,
    breed: String,
}

impl Cat {
    fn new(name: String, age: u8, breed: String) -> Self {
        Self { name, age, breed }
    }

    fn meow(&self) -> String {
        format!("{} says meow!", self.name)
    }
}
