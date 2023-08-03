use hardlight::*;

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
        let mut state = self.state.write().await;
        state.pet = Some(Pet::Dog(Dog::new(name, age, breed)));
        Ok(())
    }

    async fn become_cat(
        &self,
        name: String,
        age: u8,
        breed: String,
    ) -> HandlerResult<()> {
        let mut state = self.state.write().await;
        state.pet = Some(Pet::Cat(Cat::new(name, age, breed)));
        Ok(())
    }

    async fn make_a_noise(&self) -> HandlerResult<Option<String>> {
        let state = self.state.read().await;
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
#[derive(Debug, PartialEq, Eq, Clone)]
enum Pet {
    Dog(Dog),
    Cat(Cat),
}

#[codable]
#[derive(Debug, PartialEq, Eq, Clone)]
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
        "woof!".to_owned()
    }
}

#[codable]
#[derive(Debug, PartialEq, Eq, Clone)]
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
        "meow!".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::sleep;

    use super::*;

    #[tokio::test]
    async fn test_become_dog() {
        let config = ServerConfig::new_self_signed("localhost:8090");
        let server = Server::new(config, factory!(Handler));
        tokio::spawn(async move { server.run().await.unwrap() });
        sleep(Duration::from_millis(10)).await;

        let mut client = PetServiceClient::new_self_signed(
            "localhost:8090",
            Compression::default(),
        );
        client.connect().await.unwrap();

        let name = "Rex".to_string();
        let age = 3;
        let breed = "German Shepherd".to_string();

        let response =
            client.become_dog(name.clone(), age, breed.clone()).await;
        assert!(response.is_ok());

        let state = client.make_a_noise().await;
        assert!(state.is_ok());
        assert_eq!(state.unwrap(), Some("woof!".to_owned()));
    }

    #[tokio::test]
    async fn test_become_cat() {
        let config = ServerConfig::new_self_signed("localhost:8091");
        let server = Server::new(config, factory!(Handler));
        tokio::spawn(async move { server.run().await.unwrap() });
        sleep(Duration::from_millis(10)).await;

        let mut client = PetServiceClient::new_self_signed(
            "localhost:8091",
            Compression::default(),
        );
        client.connect().await.unwrap();

        let name = "Mittens".to_string();
        let age = 2;
        let breed = "Tabby".to_string();

        let response =
            client.become_cat(name.clone(), age, breed.clone()).await;
        assert!(response.is_ok());

        let state = client.make_a_noise().await;
        assert!(state.is_ok());
        assert_eq!(state.unwrap(), Some("meow!".to_owned()));
    }

    #[tokio::test]
    async fn test_make_a_noise_no_pet() {
        let config = ServerConfig::new_self_signed("localhost:8092");
        let server = Server::new(config, factory!(Handler));
        tokio::spawn(async move { server.run().await.unwrap() });
        sleep(Duration::from_millis(10)).await;

        let mut client = PetServiceClient::new_self_signed(
            "localhost:8092",
            Compression::default(),
        );
        client.connect().await.unwrap();

        let state = client.make_a_noise().await;
        assert!(state.is_ok());
        assert_eq!(state.unwrap(), None);
    }

    #[tokio::test]
    async fn test_server_not_running() {
        let mut client = PetServiceClient::new_self_signed(
            "localhost:8100",
            Compression::default(),
        );
        client.connect().await.unwrap_err();
    }
}
